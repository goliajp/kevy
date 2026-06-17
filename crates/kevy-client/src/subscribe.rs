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
//! let mut sub = Subscriber::open("kevy://localhost:6379", &[b"news"])?;
//! loop {
//!     if let PubsubEvent::Message { channel, payload } = sub.recv()? {
//!         println!("{}: {}", String::from_utf8_lossy(&channel),
//!                            String::from_utf8_lossy(&payload));
//!     }
//! }
//! # Ok::<(), std::io::Error>(())
//! ```

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use kevy_embedded::Subscription;
use kevy_resp::{Reply, encode_command};

#[cfg(test)]
use crate::subscribe_io::classify;
use crate::subscribe_io::{frame_to_event, invalid, recv_remote, send_to, shape};
use crate::{Target, parse_url, resolve_store};

/// One subscribed connection. Owns either a TCP socket or an in-process
/// [`Subscription`]; the variant is chosen by the URL scheme in
/// [`Subscriber::open`] / [`Subscriber::connect`].
#[derive(Debug)]
pub struct Subscriber {
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    /// TCP RESP2 connection, drained one reply at a time.
    Remote {
        stream: TcpStream,
        buf: Vec<u8>,
    },
    /// In-process bus subscription. `timeout` mirrors the TCP
    /// `SO_RCVTIMEO` behaviour for [`Subscriber::recv`] / [`Subscriber::set_read_timeout`].
    Embedded {
        subscription: Subscription,
        timeout: Option<Duration>,
    },
}

/// One pubsub frame received from the bus or the wire.
///
/// `Unsubscribe` / `Punsubscribe`'s `channel` / `pattern` is `None` when
/// the server is acknowledging "unsubscribed from everything" with a nil
/// bulk — matching the Redis wire shape.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PubsubEvent {
    /// `SUBSCRIBE` ack — one per channel the client subscribed to.
    Subscribe {
        /// Channel that was just subscribed.
        channel: Vec<u8>,
        /// Total number of channels + patterns the connection is now subscribed to.
        count: i64,
    },
    /// `PSUBSCRIBE` ack — one per pattern.
    Psubscribe {
        /// Pattern that was just subscribed.
        pattern: Vec<u8>,
        /// Total number of channels + patterns the connection is now subscribed to.
        count: i64,
    },
    /// `UNSUBSCRIBE` ack — `channel: None` when the server is reporting
    /// "no channels were subscribed" (the spec's nil bulk).
    Unsubscribe {
        /// Channel that was just unsubscribed (`None` for "all" / "none").
        channel: Option<Vec<u8>>,
        /// Total number of channels + patterns still subscribed.
        count: i64,
    },
    /// `PUNSUBSCRIBE` ack — pattern `None` when the server is reporting
    /// "no patterns were subscribed".
    Punsubscribe {
        /// Pattern that was just unsubscribed (`None` for "all" / "none").
        pattern: Option<Vec<u8>>,
        /// Total number of channels + patterns still subscribed.
        count: i64,
    },
    /// Plain `PUBLISH` delivery on a subscribed channel.
    Message {
        /// Channel the publish was made to.
        channel: Vec<u8>,
        /// Raw payload bytes (no encoding assumed).
        payload: Vec<u8>,
    },
    /// Pattern-match delivery: a `PUBLISH` to a channel that matched one
    /// of this connection's patterns.
    Pmessage {
        /// Pattern the channel matched.
        pattern: Vec<u8>,
        /// Channel the publish was made to.
        channel: Vec<u8>,
        /// Raw payload bytes.
        payload: Vec<u8>,
    },
}

impl Subscriber {
    /// Open a fresh connection without subscribing to anything yet. Call
    /// [`Self::subscribe`] / [`Self::psubscribe`] next.
    ///
    /// Accepted URLs:
    /// - `kevy://`, `redis://`, `tcp://` — TCP RESP server
    /// - `mem://<name>`, `file:///path` — in-process shared bus
    /// - `mem://` (anonymous), `rediss://`, `kevys://`, `redis://user:pass@…`
    ///   are rejected with [`io::ErrorKind::Unsupported`]
    pub fn connect(url: &str) -> io::Result<Self> {
        let target = parse_url(url)?;
        let inner = match target {
            Target::EmbedMemoryAnonymous => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "anonymous mem:// has no other producer; use mem://<name> for a shared bus",
                ));
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
                    buf: Vec::with_capacity(8192),
                }
            }
        };
        Ok(Self { inner })
    }

    /// Open and subscribe to one or more channels in one step. Returns
    /// `ErrorKind::InvalidInput` if `channels` is empty (use
    /// [`Self::connect`] for an empty start).
    pub fn open(url: &str, channels: &[&[u8]]) -> io::Result<Self> {
        if channels.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Subscriber::open needs ≥ 1 channel — use Subscriber::connect() for empty start",
            ));
        }
        let mut s = Self::connect(url)?;
        s.subscribe(channels)?;
        Ok(s)
    }

    /// `SUBSCRIBE channel [channel ...]`. Per-channel `Subscribe` acks
    /// are delivered via [`Self::recv`].
    pub fn subscribe(&mut self, channels: &[&[u8]]) -> io::Result<()> {
        if channels.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SUBSCRIBE needs ≥ 1 channel",
            ));
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
    pub fn psubscribe(&mut self, patterns: &[&[u8]]) -> io::Result<()> {
        if patterns.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PSUBSCRIBE needs ≥ 1 pattern",
            ));
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
    pub fn unsubscribe(&mut self, channels: &[&[u8]]) -> io::Result<()> {
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
    pub fn punsubscribe(&mut self, patterns: &[&[u8]]) -> io::Result<()> {
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
    pub fn recv(&mut self) -> io::Result<PubsubEvent> {
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
    pub fn recv_message(&mut self) -> io::Result<(Vec<u8>, Vec<u8>)> {
        loop {
            match self.recv()? {
                PubsubEvent::Message { channel, payload } => return Ok((channel, payload)),
                PubsubEvent::Pmessage { channel, payload, .. } => {
                    return Ok((channel, payload));
                }
                // Ack frames — keep waiting for the next real message.
                PubsubEvent::Subscribe { .. }
                | PubsubEvent::Psubscribe { .. }
                | PubsubEvent::Unsubscribe { .. }
                | PubsubEvent::Punsubscribe { .. } => {}
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
    /// [`io::ErrorKind::Unsupported`].
    ///
    /// Must be called BEFORE any [`Self::subscribe`] /
    /// [`Self::psubscribe`] — Redis requires `HELLO` be the first
    /// command on a connection that uses it.
    pub fn hello3(&mut self) -> io::Result<PubsubEvent> {
        match &mut self.inner {
            Inner::Embedded { .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "HELLO 3 is a remote/TCP-only operation; embedded backend has no proto switch",
            )),
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
                    match kevy_resp::parse_reply(buf) {
                        Ok(Some((reply, used))) => {
                            buf.drain(..used);
                            // Reply::Map / Reply::Array both acceptable
                            // (a server that rejected V3 would emit an
                            // Error reply — fall through to the error
                            // branch below).
                            return match reply {
                                Reply::Map(_) | Reply::Array(_) => {
                                    Ok(PubsubEvent::Subscribe {
                                        channel: b"HELLO".to_vec(),
                                        count: 3,
                                    })
                                }
                                Reply::Error(e) => Err(io::Error::other(
                                    String::from_utf8_lossy(&e).into_owned(),
                                )),
                                other => Err(invalid(format!(
                                    "unexpected HELLO 3 reply shape: {}",
                                    shape(&other)
                                ))),
                            };
                        }
                        Ok(None) => {}
                        Err(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "malformed HELLO 3 reply",
                            ));
                        }
                    }
                    let n = stream.read(&mut chunk)?;
                    if n == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "server closed connection during HELLO 3",
                        ));
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }

    /// Apply (or clear) a read timeout. After setting `Some(dur)`,
    /// [`Self::recv`] returns an `io::Error` of kind `WouldBlock` /
    /// `TimedOut` when no frame arrives within `dur`.
    pub fn set_read_timeout(&mut self, dur: Option<Duration>) -> io::Result<()> {
        match &mut self.inner {
            Inner::Remote { stream, .. } => stream.set_read_timeout(dur),
            Inner::Embedded { timeout, .. } => {
                *timeout = dur;
                Ok(())
            }
        }
    }

    /// Borrowing iterator over every pubsub frame — ack frames included.
    /// Each `next()` is one blocking [`Self::recv`]. Terminates (`None`)
    /// when the underlying stream / bus is gone (`ErrorKind::UnexpectedEof`);
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
    type Item = io::Result<PubsubEvent>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.sub.recv() {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => None,
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
    type Item = io::Result<(Vec<u8>, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.sub.recv_message() {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => None,
            other => Some(other),
        }
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

fn remote_host_port(url: &str) -> io::Result<(String, u16)> {
    let (_scheme, rest) = url.split_once("://").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "URL missing '://'")
    })?;
    if rest.contains('@') {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "userinfo (user:pass@host) is unsupported — kevy has no AUTH",
        ));
    }
    let authority = rest.split('/').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p.parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, format!("bad port: {p}"))
            })?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), 6379),
    };
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty host"));
    }
    Ok((host, port))
}

#[cfg(test)]
#[path = "subscribe_tests.rs"]
mod tests;
