//! Pub/sub consumer side — a connection dedicated to receiving messages.
//!
//! `SUBSCRIBE` / `PSUBSCRIBE` morph a connection into a one-way event
//! stream: the client no longer sends ordinary commands and instead reads
//! an unbounded sequence of `subscribe`, `message`, `pmessage`,
//! `unsubscribe`, … frames until the connection is closed. That semantic
//! doesn't fit the one-shot `Connection::request` shape, so subscribed
//! traffic gets its own type, [`Subscriber`].
//!
//! Two backends, switched on the URL:
//! - `kevy://` / `redis://` / `tcp://` — dedicated TCP socket
//! - `mem://<name>` / `file:///path` — in-process bus, via the URL
//!   registry in [`crate::resolve_store`]. Anonymous `mem://` (no name)
//!   has no bus and is rejected; use a named bus to actually receive
//!   messages from a [`crate::Connection::publish`] on the same URL.
//!
//! ```no_run
//! use kevy_client::{Subscriber, PubsubEvent};
//!
//! let mut sub = Subscriber::connect_channels("kevy://localhost:6379", &[b"news"])?;
//! loop {
//!     if let PubsubEvent::Message { channel, payload } = sub.recv()? {
//!         println!("{}: {}", String::from_utf8_lossy(&channel),
//!                            String::from_utf8_lossy(&payload));
//!     }
//! }
//! # Ok::<(), kevy_client::KevyError>(())
//! ```

use crate::{KevyError, KevyResult};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use kevy_embedded::Subscription;
use kevy_resp::{Reply, encode_command};
use kevy_resp_client::ReplyReadBuf;

use crate::subscribe_io::{frame_to_event, invalid, recv_remote, send_to, shape};
use crate::{Target, parse_url, resolve_store};

/// One subscribed connection. Owns either a TCP socket or an in-process
/// [`Subscription`]; the variant is chosen by the URL scheme in
/// [`Subscriber::connect_channels`] / [`Subscriber::connect`].
#[derive(Debug)]
pub struct Subscriber {
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    /// TCP RESP2 connection, drained one reply at a time.
    Remote {
        stream: TcpStream,
        buf: ReplyReadBuf,
    },
    /// In-process bus subscription. `timeout` mirrors the TCP
    /// `SO_RCVTIMEO` behaviour for [`Subscriber::recv`] / [`Subscriber::set_read_timeout`].
    Embedded {
        subscription: Subscription,
        timeout: Option<Duration>,
    },
}

// The pubsub frame vocabulary is canonical in `kevy_resp_client`,
// shared with the async client — one enum, no per-crate mirrors.
pub use kevy_resp_client::PubsubEvent;

impl Subscriber {
    /// Open a fresh connection without subscribing to anything yet. Call
    /// [`Self::subscribe`] / [`Self::psubscribe`] next.
    ///
    /// Accepted URLs:
    /// - `kevy://`, `redis://`, `tcp://` — TCP RESP server
    /// - `mem://<name>`, `file:///path` — in-process shared bus
    /// - `mem://` (anonymous), `rediss://`, `kevys://`, `redis://user:pass@…`
    ///   are rejected with [`KevyError::Unsupported`]
    pub fn connect(url: &str) -> KevyResult<Self> {
        let target = parse_url(url)?;
        let inner = match target {
            Target::EmbedMemoryAnonymous => {
                return Err(KevyError::Unsupported("anonymous mem:// has no other producer; use mem://<name> for a shared bus".into()));
            }
            Target::EmbedMemoryNamed(_) | Target::EmbedPersist(_) => Inner::Embedded {
                subscription: resolve_store(&target)?.subscribe(&[]),
                timeout: None,
            },
            Target::Remote(remote_url) => {
                let (host, port) = remote_host_port(&remote_url)?;
                let stream = TcpStream::connect((host.as_str(), port))?;
                stream.set_nodelay(true).ok();
                Inner::Remote {
                    stream,
                    buf: ReplyReadBuf::with_capacity(8192),
                }
            }
        };
        Ok(Self { inner })
    }

    /// Connect and subscribe to one or more channels in one step. Returns
    /// `ErrorKind::InvalidInput` if `channels` is empty (use
    /// [`Self::connect`] for an empty start).
    pub fn connect_channels(url: &str, channels: &[&[u8]]) -> KevyResult<Self> {
        if channels.is_empty() {
            return Err(KevyError::InvalidInput("Subscriber::connect_channels needs ≥ 1 channel — use Subscriber::connect() for empty start".into()));
        }
        let mut s = Self::connect(url)?;
        s.subscribe(channels)?;
        Ok(s)
    }

    /// `SUBSCRIBE channel [channel ...]`. Per-channel `Subscribe` acks
    /// are delivered via [`Self::recv`].
    pub fn subscribe(&mut self, channels: &[&[u8]]) -> KevyResult<()> {
        if channels.is_empty() {
            return Err(KevyError::InvalidInput("SUBSCRIBE needs ≥ 1 channel".into()));
        }
        match &mut self.inner {
            Inner::Remote { stream, .. } => send_to(stream, b"SUBSCRIBE", channels),
            Inner::Embedded { subscription, .. } => {
                subscription.subscribe(channels);
                Ok(())
            }
        }
    }

    /// `PSUBSCRIBE pattern [pattern ...]`. Patterns use Redis glob syntax
    /// (`*`, `?`, `[…]`).
    pub fn psubscribe(&mut self, patterns: &[&[u8]]) -> KevyResult<()> {
        if patterns.is_empty() {
            return Err(KevyError::InvalidInput("PSUBSCRIBE needs ≥ 1 pattern".into()));
        }
        match &mut self.inner {
            Inner::Remote { stream, .. } => send_to(stream, b"PSUBSCRIBE", patterns),
            Inner::Embedded { subscription, .. } => {
                subscription.psubscribe(patterns);
                Ok(())
            }
        }
    }

    /// `UNSUBSCRIBE [channel ...]`. Empty `channels` unsubscribes from
    /// every channel (Redis wire semantics).
    pub fn unsubscribe(&mut self, channels: &[&[u8]]) -> KevyResult<()> {
        match &mut self.inner {
            Inner::Remote { stream, .. } => send_to(stream, b"UNSUBSCRIBE", channels),
            Inner::Embedded { subscription, .. } => {
                subscription.unsubscribe(channels);
                Ok(())
            }
        }
    }

    /// `PUNSUBSCRIBE [pattern ...]`. Empty `patterns` unsubscribes from
    /// every pattern.
    pub fn punsubscribe(&mut self, patterns: &[&[u8]]) -> KevyResult<()> {
        match &mut self.inner {
            Inner::Remote { stream, .. } => send_to(stream, b"PUNSUBSCRIBE", patterns),
            Inner::Embedded { subscription, .. } => {
                subscription.punsubscribe(patterns);
                Ok(())
            }
        }
    }

    /// Block until the next pubsub frame arrives. Apply
    /// [`Self::set_read_timeout`] for bounded blocking.
    /// Connection close / bus tear-down yields `ErrorKind::UnexpectedEof`.
    pub fn recv(&mut self) -> KevyResult<PubsubEvent> {
        match &mut self.inner {
            Inner::Remote { stream, buf } => recv_remote(stream, buf),
            Inner::Embedded {
                subscription,
                timeout,
            } => {
                let frame = match *timeout {
                    Some(d) => subscription.recv_timeout(d)?,
                    None => subscription.recv()?,
                };
                Ok(frame_to_event(frame))
            }
        }
    }

    /// Block until the next published `Message` / `Pmessage` arrives,
    /// silently skipping subscription-acknowledgement frames
    /// ([`PubsubEvent::Subscribe`] / [`PubsubEvent::Unsubscribe`] /
    /// [`PubsubEvent::Psubscribe`] / [`PubsubEvent::Punsubscribe`]) along
    /// the way.
    ///
    /// This is the form most callers want — almost no consumer of
    /// pubsub needs to see the ack frames (they're a wire-protocol
    /// detail), so a loop+match around [`Self::recv`] is essentially
    /// boilerplate. Returns `(channel, payload)`. For pattern matches,
    /// `channel` is the concrete channel the publisher used (matching
    /// Redis's `pmessage` shape, where `pattern` is discarded — use
    /// [`Self::recv`] directly if you need it).
    ///
    /// Errors from [`Self::recv`] (connection close, timeout, etc.)
    /// propagate unchanged.
    pub fn recv_message(&mut self) -> KevyResult<(Vec<u8>, Vec<u8>)> {
        loop {
            match self.recv()? {
                PubsubEvent::Message { channel, payload } => return Ok((channel, payload)),
                PubsubEvent::Pmessage { channel, payload, .. } => {
                    return Ok((channel, payload));
                }
                // Ack frames (and any frame kind a future protocol
                // adds — the enum is non-exhaustive) — keep waiting
                // for the next real message.
                _ => {}
            }
        }
    }

    /// Negotiate RESP3 on this connection by sending `HELLO 3` and
    /// draining the ack. Subsequent `SUBSCRIBE` / `PSUBSCRIBE` /
    /// `PUBLISH` deliveries arrive as push frames (`>N\r\n…`) instead
    /// of the legacy RESP2 array shape (`*N\r\n…`); [`Self::recv`]
    /// accepts both transparently, so existing code keeps working with
    /// no other changes.
    ///
    /// Remote-only: the embedded backend has no proto negotiation
    /// concept (frames go through the in-process bus typed). Calling
    /// `hello3` on an embedded [`Subscriber`] returns
    /// [`KevyError::Unsupported`].
    ///
    /// Must be called BEFORE any [`Self::subscribe`] /
    /// [`Self::psubscribe`] — Redis requires `HELLO` be the first
    /// command on a connection that uses it.
    pub fn hello3(&mut self) -> KevyResult<PubsubEvent> {
        match &mut self.inner {
            Inner::Embedded { .. } => Err(KevyError::Unsupported("HELLO 3 is a remote/TCP-only operation; embedded backend has no proto switch".into())),
            Inner::Remote { stream, buf } => {
                let mut frame = Vec::new();
                encode_command(&mut frame, &[b"HELLO".to_vec(), b"3".to_vec()]);
                stream.write_all(&frame)?;
                // The HELLO 3 ack itself comes back as a RESP3 Map
                // (`%7\r\n…`). parse_reply accepts it (P1); we drain
                // and discard since the proto switch is the actual
                // semantic — the body's just server metadata.
                let mut chunk = [0u8; 4096];
                loop {
                    match buf.parse_next() {
                        Ok(Some(reply)) => return classify_hello3_reply(reply),
                        Ok(None) => {}
                        Err(_) => {
                            return Err(KevyError::Protocol("malformed HELLO 3 reply".into()));
                        }
                    }
                    let n = stream.read(&mut chunk)?;
                    if n == 0 {
                        return Err(KevyError::Closed);
                    }
                    buf.extend(&chunk[..n]);
                }
            }
        }
    }

    /// Apply (or clear) a read timeout. After setting `Some(dur)`,
    /// [`Self::recv`] returns [`KevyError::Io`] with kind `WouldBlock` /
    /// `TimedOut` when no frame arrives within `dur`.
    pub fn set_read_timeout(&mut self, dur: Option<Duration>) -> KevyResult<()> {
        match &mut self.inner {
            Inner::Remote { stream, .. } => Ok(stream.set_read_timeout(dur)?),
            Inner::Embedded { timeout, .. } => {
                *timeout = dur;
                Ok(())
            }
        }
    }

    /// Borrowing iterator over every pubsub frame — ack frames included.
    /// Each `next()` is one blocking [`Self::recv`]. Terminates (`None`)
    /// when the underlying stream / bus is gone ([`KevyError::Closed`]);
    /// every other error is surfaced as `Some(Err(_))` so the caller can
    /// decide whether to retry (e.g. a read timeout) or break.
    ///
    /// kevy stays 0-deps so this is a `std::iter::Iterator`, not a
    /// `futures::Stream`. Async runtimes consume it via
    /// `spawn_blocking` (see `docs/pubsub.md`).
    pub fn events(&mut self) -> SubscriberEvents<'_> {
        SubscriberEvents { sub: self }
    }

    /// Borrowing iterator that silently skips `(p)?(un)?subscribe` acks
    /// and yields the payload tuples consumers actually want. Mirrors
    /// [`Self::recv_message`] in iterator form. For `Pmessage` the
    /// pattern is discarded — fall back to [`Self::events`] if you need it.
    pub fn messages(&mut self) -> SubscriberMessages<'_> {
        SubscriberMessages { sub: self }
    }
}

/// Iterator returned by [`Subscriber::events`]. Yields every pubsub
/// frame (acks + payloads). See the method docs for termination + error
/// semantics.
#[derive(Debug)]
pub struct SubscriberEvents<'a> {
    sub: &'a mut Subscriber,
}

impl Iterator for SubscriberEvents<'_> {
    type Item = KevyResult<PubsubEvent>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.sub.recv() {
            Err(KevyError::Closed) => None,
            other => Some(other),
        }
    }
}

/// Iterator returned by [`Subscriber::messages`]. Yields one
/// `(channel, payload)` per published `message` / `pmessage`; ack frames
/// are silently consumed and not yielded.
#[derive(Debug)]
pub struct SubscriberMessages<'a> {
    sub: &'a mut Subscriber,
}

impl Iterator for SubscriberMessages<'_> {
    type Item = KevyResult<(Vec<u8>, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.sub.recv_message() {
            Err(KevyError::Closed) => None,
            other => Some(other),
        }
    }
}

/// Classify the drained `HELLO 3` ack. Reply::Map / Reply::Array are
/// both acceptable (a server that rejected V3 would emit an Error
/// reply — surfaced via the error branch below).
fn classify_hello3_reply(reply: Reply) -> KevyResult<PubsubEvent> {
    match reply {
        Reply::Map(_) | Reply::Array(_) => Ok(PubsubEvent::Subscribe {
            channel: b"HELLO".to_vec(),
            count: 3,
        }),
        Reply::Error(e) => Err(KevyError::Protocol(String::from_utf8_lossy(&e).into_owned())),
        other => Err(invalid(format!(
            "unexpected HELLO 3 reply shape: {}",
            shape(&other)
        ))),
    }
}

// `send_to` / `recv_remote` / `frame_to_event` / `classify` and the
// per-field reply unwrap helpers live in [`crate::subscribe_io`] —
// split out so this file stays under the 500-LOC house rule.

// ─────────────────────────────────────────────────────────────────────────
// Remote host:port extraction. Reuses the same authority parsing logic
// kevy-resp-client::from_url applies, but only needs host+port (pub/sub
// is global, not db-scoped — any /N path segment is ignored).
// ─────────────────────────────────────────────────────────────────────────

fn remote_host_port(url: &str) -> KevyResult<(String, u16)> {
    let (_scheme, rest) = url.split_once("://").ok_or_else(|| {
        KevyError::InvalidInput("URL missing '://'".into())
    })?;
    if rest.contains('@') {
        return Err(KevyError::Unsupported("userinfo (user:pass@host) is unsupported — kevy has no AUTH".into()));
    }
    let authority = rest.split('/').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p.parse().map_err(|_| {
                KevyError::InvalidInput(format!("bad port: {p}"))
            })?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), 6379),
    };
    if host.is_empty() {
        return Err(KevyError::InvalidInput("empty host".into()));
    }
    Ok((host, port))
}

#[cfg(test)]
#[path = "subscribe_tests.rs"]
mod tests;
