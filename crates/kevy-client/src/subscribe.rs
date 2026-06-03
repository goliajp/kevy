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

use kevy_embedded::{PubsubFrame, Subscription};
use kevy_resp::{Reply, encode_command, parse_reply};

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
    /// ([`PubsubEvent::Subscribe`] / [`Unsubscribe`] / [`Psubscribe`] /
    /// [`Punsubscribe`]) along the way.
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
                | PubsubEvent::Punsubscribe { .. } => continue,
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
}

fn send_to(stream: &mut TcpStream, verb: &[u8], args: &[&[u8]]) -> io::Result<()> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(verb.to_vec());
    argv.extend(args.iter().map(|a| a.to_vec()));
    let mut frame = Vec::new();
    encode_command(&mut frame, &argv);
    stream.write_all(&frame)
}

fn recv_remote(stream: &mut TcpStream, buf: &mut Vec<u8>) -> io::Result<PubsubEvent> {
    let mut chunk = [0u8; 8192];
    loop {
        match parse_reply(buf) {
            Ok(Some((reply, used))) => {
                buf.drain(..used);
                return classify(reply);
            }
            Ok(None) => {}
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed reply",
                ));
            }
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed connection",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn frame_to_event(frame: PubsubFrame) -> PubsubEvent {
    match frame {
        PubsubFrame::Subscribe { channel, count } => PubsubEvent::Subscribe {
            channel,
            count: count as i64,
        },
        PubsubFrame::Psubscribe { pattern, count } => PubsubEvent::Psubscribe {
            pattern,
            count: count as i64,
        },
        PubsubFrame::Unsubscribe { channel, count } => PubsubEvent::Unsubscribe {
            channel,
            count: count as i64,
        },
        PubsubFrame::Punsubscribe { pattern, count } => PubsubEvent::Punsubscribe {
            pattern,
            count: count as i64,
        },
        PubsubFrame::Message { channel, payload } => PubsubEvent::Message { channel, payload },
        PubsubFrame::Pmessage {
            pattern,
            channel,
            payload,
        } => PubsubEvent::Pmessage {
            pattern,
            channel,
            payload,
        },
    }
}

fn classify(reply: Reply) -> io::Result<PubsubEvent> {
    let items = match reply {
        Reply::Array(v) => v,
        other => return Err(invalid(format!("expected array frame, got {}", shape(&other)))),
    };
    let kind = match items.first() {
        Some(Reply::Bulk(b)) => b.clone(),
        _ => return Err(invalid("pubsub frame missing kind field")),
    };
    match kind.as_slice() {
        b"subscribe" => {
            let [_, ch, n] = into_array3(items)?;
            Ok(PubsubEvent::Subscribe {
                channel: take_bulk(ch, "channel")?,
                count: take_int(n, "count")?,
            })
        }
        b"psubscribe" => {
            let [_, p, n] = into_array3(items)?;
            Ok(PubsubEvent::Psubscribe {
                pattern: take_bulk(p, "pattern")?,
                count: take_int(n, "count")?,
            })
        }
        b"unsubscribe" => {
            let [_, ch, n] = into_array3(items)?;
            Ok(PubsubEvent::Unsubscribe {
                channel: take_bulk_or_nil(ch, "channel")?,
                count: take_int(n, "count")?,
            })
        }
        b"punsubscribe" => {
            let [_, p, n] = into_array3(items)?;
            Ok(PubsubEvent::Punsubscribe {
                pattern: take_bulk_or_nil(p, "pattern")?,
                count: take_int(n, "count")?,
            })
        }
        b"message" => {
            let [_, ch, payload] = into_array3(items)?;
            Ok(PubsubEvent::Message {
                channel: take_bulk(ch, "channel")?,
                payload: take_bulk(payload, "payload")?,
            })
        }
        b"pmessage" => {
            let [_, pat, ch, payload] = into_array4(items)?;
            Ok(PubsubEvent::Pmessage {
                pattern: take_bulk(pat, "pattern")?,
                channel: take_bulk(ch, "channel")?,
                payload: take_bulk(payload, "payload")?,
            })
        }
        other => Err(invalid(format!(
            "unknown pubsub kind '{}'",
            String::from_utf8_lossy(other)
        ))),
    }
}

fn into_array3(items: Vec<Reply>) -> io::Result<[Reply; 3]> {
    items.try_into().map_err(|v: Vec<Reply>| {
        invalid(format!("expected 3-element pubsub frame, got {}", v.len()))
    })
}

fn into_array4(items: Vec<Reply>) -> io::Result<[Reply; 4]> {
    items.try_into().map_err(|v: Vec<Reply>| {
        invalid(format!("expected 4-element pubsub frame, got {}", v.len()))
    })
}

fn take_bulk(r: Reply, field: &str) -> io::Result<Vec<u8>> {
    match r {
        Reply::Bulk(b) => Ok(b),
        other => Err(invalid(format!(
            "expected bulk for {field}, got {}",
            shape(&other)
        ))),
    }
}

fn take_bulk_or_nil(r: Reply, field: &str) -> io::Result<Option<Vec<u8>>> {
    match r {
        Reply::Bulk(b) => Ok(Some(b)),
        Reply::Nil => Ok(None),
        other => Err(invalid(format!(
            "expected bulk/nil for {field}, got {}",
            shape(&other)
        ))),
    }
}

fn take_int(r: Reply, field: &str) -> io::Result<i64> {
    match r {
        Reply::Int(n) => Ok(n),
        other => Err(invalid(format!(
            "expected integer for {field}, got {}",
            shape(&other)
        ))),
    }
}

fn shape(r: &Reply) -> &'static str {
    match r {
        Reply::Simple(_) => "simple-string",
        Reply::Error(_) => "error",
        Reply::Int(_) => "integer",
        Reply::Bulk(_) => "bulk-string",
        Reply::Nil => "nil",
        Reply::Array(_) => "array",
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

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
