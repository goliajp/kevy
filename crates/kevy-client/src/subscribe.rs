//! Pub/sub consumer side — a TCP connection dedicated to receiving messages.
//!
//! `SUBSCRIBE` / `PSUBSCRIBE` morph a Redis/kevy connection into a one-way
//! event stream: the client no longer sends ordinary commands and instead
//! reads an unbounded sequence of `subscribe`, `message`, `pmessage`,
//! `unsubscribe`, … frames until the connection is closed. That semantic
//! doesn't fit the one-shot `Connection::request` shape — so subscribed
//! traffic gets its own type, [`Subscriber`], on its own socket.
//!
//! ```no_run
//! use kevy_client::Subscriber;
//!
//! let mut sub = Subscriber::open("kevy://localhost:6379", &[b"news"])?;
//! loop {
//!     match sub.recv()? {
//!         kevy_client::PubsubEvent::Message { channel, payload } => {
//!             println!("{}: {}", String::from_utf8_lossy(&channel),
//!                                String::from_utf8_lossy(&payload));
//!         }
//!         _ => {}  // ignore subscribe-acks and other meta frames
//!     }
//! }
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! `mem://` / `file://` URLs are rejected with `ErrorKind::Unsupported`:
//! single-process embed has no other producer to receive messages from.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use kevy_resp::{Reply, encode_command, parse_reply};

/// One subscribed TCP connection. Owns the socket; not `Sync`.
#[derive(Debug)]
pub struct Subscriber {
    stream: TcpStream,
    buf: Vec<u8>,
}

/// One pubsub frame received from the server.
///
/// `Unsubscribe` / `Punsubscribe`'s `channel` / `pattern` is `None` when the
/// server is acknowledging "unsubscribed from everything" with a nil bulk
/// — matching the Redis wire shape.
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
    /// Open a fresh TCP connection without subscribing to anything.
    /// Use [`Self::subscribe`] / [`Self::psubscribe`] next.
    ///
    /// Accepted URL schemes: `kevy://`, `redis://`, `tcp://` (all wire-identical).
    /// `mem://` / `file://` return `ErrorKind::Unsupported` — there is no
    /// other process to receive messages from inside an embedded store.
    pub fn connect(url: &str) -> io::Result<Self> {
        let (host, port) = parse_pubsub_url(url)?;
        let stream = TcpStream::connect((host.as_str(), port))?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            buf: Vec::with_capacity(8192),
        })
    }

    /// Open and subscribe to one or more channels in one step. After the
    /// call returns, the server has the `SUBSCRIBE` command queued — drain
    /// the per-channel ack frames with [`Self::recv`] before
    /// you act on `Message` events.
    ///
    /// Returns `ErrorKind::InvalidInput` if `channels` is empty (use
    /// [`Self::connect`] + [`Self::psubscribe`] for a pattern-only start).
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

    /// `SUBSCRIBE channel [channel ...]`. Returns once the bytes are written;
    /// the server sends one `Subscribe` ack per channel — drain with
    /// [`Self::recv`].
    pub fn subscribe(&mut self, channels: &[&[u8]]) -> io::Result<()> {
        if channels.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SUBSCRIBE needs ≥ 1 channel",
            ));
        }
        self.send(b"SUBSCRIBE", channels)
    }

    /// `PSUBSCRIBE pattern [pattern ...]`. Patterns use Redis glob syntax
    /// (`*`, `?`, `[…]`). Same ack-draining note as [`Self::subscribe`].
    pub fn psubscribe(&mut self, patterns: &[&[u8]]) -> io::Result<()> {
        if patterns.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PSUBSCRIBE needs ≥ 1 pattern",
            ));
        }
        self.send(b"PSUBSCRIBE", patterns)
    }

    /// `UNSUBSCRIBE [channel ...]`. Empty `channels` unsubscribes from
    /// every channel (Redis wire semantics).
    pub fn unsubscribe(&mut self, channels: &[&[u8]]) -> io::Result<()> {
        self.send(b"UNSUBSCRIBE", channels)
    }

    /// `PUNSUBSCRIBE [pattern ...]`. Empty `patterns` unsubscribes from
    /// every pattern.
    pub fn punsubscribe(&mut self, patterns: &[&[u8]]) -> io::Result<()> {
        self.send(b"PUNSUBSCRIBE", patterns)
    }

    /// Block until the next pubsub frame arrives, parse it, classify it.
    ///
    /// `recv` itself never times out — apply a read timeout via
    /// [`Self::set_read_timeout`] if you need bounded blocking.
    /// Server close yields `ErrorKind::UnexpectedEof`; a malformed RESP
    /// frame yields `ErrorKind::InvalidData`.
    pub fn recv(&mut self) -> io::Result<PubsubEvent> {
        let mut chunk = [0u8; 8192];
        loop {
            match parse_reply(&self.buf) {
                Ok(Some((reply, used))) => {
                    self.buf.drain(..used);
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
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed connection",
                ));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Apply (or clear) a read timeout on the underlying socket.
    /// After setting `Some(dur)`, [`Self::recv`] will return an `io::Error`
    /// of kind `WouldBlock` / `TimedOut` if no data arrives within `dur`.
    pub fn set_read_timeout(&mut self, dur: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(dur)
    }

    fn send(&mut self, verb: &[u8], args: &[&[u8]]) -> io::Result<()> {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(verb.to_vec());
        argv.extend(args.iter().map(|a| a.to_vec()));
        let mut frame = Vec::new();
        encode_command(&mut frame, &argv);
        self.stream.write_all(&frame)
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
// URL parsing (kevy://, redis://, tcp://; rejects mem/file/rediss/userinfo)
// ─────────────────────────────────────────────────────────────────────────

fn parse_pubsub_url(url: &str) -> io::Result<(String, u16)> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "URL missing '://'")
    })?;
    match scheme {
        "kevy" | "redis" | "tcp" => {}
        "mem" | "file" => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{scheme}:// is an embedded backend — pub/sub needs a TCP server. \
                     Use kevy://host:port instead."
                ),
            ));
        }
        "rediss" | "kevys" => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "TLS schemes (rediss://, kevys://) are unsupported — kevy has no TLS",
            ));
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown URL scheme '{other}://'"),
            ));
        }
    }
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
mod tests {
    use super::*;

    // ----- URL parsing -----

    #[test]
    fn parses_kevy_redis_tcp() {
        for url in [
            "kevy://localhost:6379",
            "redis://localhost:6379",
            "tcp://localhost:6379",
        ] {
            let (h, p) = parse_pubsub_url(url).unwrap();
            assert_eq!(h, "localhost");
            assert_eq!(p, 6379);
        }
    }

    #[test]
    fn default_port_when_omitted() {
        let (h, p) = parse_pubsub_url("kevy://example.com").unwrap();
        assert_eq!(h, "example.com");
        assert_eq!(p, 6379);
    }

    #[test]
    fn db_path_segment_ignored() {
        // Pub/sub is global, not db-scoped — `/N` is accepted but discarded.
        let (h, p) = parse_pubsub_url("kevy://h:1234/0").unwrap();
        assert_eq!(h, "h");
        assert_eq!(p, 1234);
        let (h, p) = parse_pubsub_url("redis://h:1234/3").unwrap();
        assert_eq!(h, "h");
        assert_eq!(p, 1234);
    }

    #[test]
    fn mem_file_rejected_unsupported() {
        for url in ["mem://", "file:///tmp/data"] {
            let err = parse_pubsub_url(url).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        }
    }

    #[test]
    fn tls_rejected_unsupported() {
        assert_eq!(
            parse_pubsub_url("rediss://h:6379").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn userinfo_rejected_unsupported() {
        assert_eq!(
            parse_pubsub_url("kevy://u:p@h:6379").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn unknown_scheme_rejected() {
        assert_eq!(
            parse_pubsub_url("memcached://h:11211").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn bad_port_rejected() {
        assert!(parse_pubsub_url("kevy://h:notaport").is_err());
        assert!(parse_pubsub_url("kevy://h:99999").is_err());
    }

    #[test]
    fn empty_host_rejected() {
        assert!(parse_pubsub_url("kevy://:6379").is_err());
    }

    // ----- classify -----

    #[test]
    fn classify_subscribe_ack() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"subscribe".to_vec()),
            Reply::Bulk(b"chan".to_vec()),
            Reply::Int(1),
        ]);
        assert_eq!(
            classify(r).unwrap(),
            PubsubEvent::Subscribe {
                channel: b"chan".to_vec(),
                count: 1,
            }
        );
    }

    #[test]
    fn classify_psubscribe_ack() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"psubscribe".to_vec()),
            Reply::Bulk(b"chan.*".to_vec()),
            Reply::Int(2),
        ]);
        assert_eq!(
            classify(r).unwrap(),
            PubsubEvent::Psubscribe {
                pattern: b"chan.*".to_vec(),
                count: 2,
            }
        );
    }

    #[test]
    fn classify_message_event() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"message".to_vec()),
            Reply::Bulk(b"news".to_vec()),
            Reply::Bulk(b"hello".to_vec()),
        ]);
        assert_eq!(
            classify(r).unwrap(),
            PubsubEvent::Message {
                channel: b"news".to_vec(),
                payload: b"hello".to_vec(),
            }
        );
    }

    #[test]
    fn classify_pmessage_event() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"pmessage".to_vec()),
            Reply::Bulk(b"news.*".to_vec()),
            Reply::Bulk(b"news.tech".to_vec()),
            Reply::Bulk(b"hi".to_vec()),
        ]);
        assert_eq!(
            classify(r).unwrap(),
            PubsubEvent::Pmessage {
                pattern: b"news.*".to_vec(),
                channel: b"news.tech".to_vec(),
                payload: b"hi".to_vec(),
            }
        );
    }

    #[test]
    fn classify_unsubscribe_with_channel() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"unsubscribe".to_vec()),
            Reply::Bulk(b"chan".to_vec()),
            Reply::Int(0),
        ]);
        assert_eq!(
            classify(r).unwrap(),
            PubsubEvent::Unsubscribe {
                channel: Some(b"chan".to_vec()),
                count: 0,
            }
        );
    }

    #[test]
    fn classify_unsubscribe_with_nil_channel() {
        // Spec: when there were no subscribed channels, the server replies
        // with a nil bulk in the channel slot.
        let r = Reply::Array(vec![
            Reply::Bulk(b"unsubscribe".to_vec()),
            Reply::Nil,
            Reply::Int(0),
        ]);
        assert_eq!(
            classify(r).unwrap(),
            PubsubEvent::Unsubscribe {
                channel: None,
                count: 0,
            }
        );
    }

    #[test]
    fn classify_punsubscribe_with_pattern() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"punsubscribe".to_vec()),
            Reply::Bulk(b"chan.*".to_vec()),
            Reply::Int(0),
        ]);
        assert_eq!(
            classify(r).unwrap(),
            PubsubEvent::Punsubscribe {
                pattern: Some(b"chan.*".to_vec()),
                count: 0,
            }
        );
    }

    #[test]
    fn classify_rejects_unknown_kind() {
        let r = Reply::Array(vec![
            Reply::Bulk(b"bogus".to_vec()),
            Reply::Bulk(b"x".to_vec()),
            Reply::Int(0),
        ]);
        assert_eq!(classify(r).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn classify_rejects_non_array() {
        assert_eq!(
            classify(Reply::Simple(b"OK".to_vec())).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn classify_rejects_wrong_arity() {
        // subscribe with 2 elements (missing count).
        let r = Reply::Array(vec![
            Reply::Bulk(b"subscribe".to_vec()),
            Reply::Bulk(b"x".to_vec()),
        ]);
        assert_eq!(classify(r).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    // ----- subscribe arg validation -----

    #[test]
    fn open_with_empty_channels_rejected() {
        let err = Subscriber::open("kevy://127.0.0.1:1", &[]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
