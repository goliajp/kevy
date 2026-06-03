//! Blocking RESP2 client over `TcpStream`.
//!
//! [`RespClient::connect`] opens a TCP connection (with `TCP_NODELAY`);
//! [`RespClient::request`] writes one command and blocks until exactly one
//! reply is parsed. Works against any RESP2 server — kevy, valkey, redis.
//!
//! [`RespClient::from_url`] is the URL-string entry point and accepts
//! `kevy://` (kevy-native alias), `redis://` (standard), and `tcp://`
//! (plain host:port — no leading SELECT round-trip):
//!
//! ```no_run
//! # use kevy_resp_client::RespClient;
//! let _ = RespClient::from_url("kevy://localhost:6379")?;     // alias of redis://
//! let _ = RespClient::from_url("kevy://localhost:6379/0")?;   // also issues SELECT 0
//! let _ = RespClient::from_url("redis://10.0.0.5:6379")?;
//! let _ = RespClient::from_url("tcp://kevy.internal:6379")?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! Single-threaded; one client per thread. Holds an incremental read buffer
//! so multi-segment replies reassemble across `read` calls.
//!
//! Pure Rust, only deps are `std` + [`kevy_resp`].
//!
//! # Example
//!
//! ```no_run
//! use kevy_resp_client::RespClient;
//!
//! let mut c = RespClient::connect("127.0.0.1", 6379)?;
//! let reply = c.request(&[b"PING".to_vec()])?;
//! println!("{reply:?}");
//! # Ok::<(), std::io::Error>(())
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub use kevy_resp::Reply;
use kevy_resp::{encode_command, parse_reply};
use std::io::{self, Read, Write};
use std::net::TcpStream;

/// A blocking RESP2 connection over `TcpStream`.
///
/// Holds the stream plus an incremental read buffer so multi-segment replies
/// reassemble across `read` calls. Not `Sync`; one client per thread.
pub struct RespClient {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl RespClient {
    /// Connect to `host:port`, enabling `TCP_NODELAY` (best-effort).
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        let stream = TcpStream::connect((host, port))?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream,
            buf: Vec::with_capacity(8192),
        })
    }

    /// Send one command (`args` is RESP-encoded as a multibulk array) and
    /// block until exactly one reply is parsed. Returns the parsed [`Reply`].
    pub fn request(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        let mut out = Vec::new();
        encode_command(&mut out, args);
        self.stream.write_all(&out)?;

        let mut chunk = [0u8; 8192];
        loop {
            match parse_reply(&self.buf) {
                Ok(Some((reply, used))) => {
                    self.buf.drain(..used);
                    return Ok(reply);
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

    /// Connect from a URL string.
    ///
    /// Accepted schemes (all wire-protocol identical — RESP2 over TCP):
    /// - `kevy://host[:port][/db]` — kevy-native alias of `redis://`.
    /// - `redis://host[:port][/db]` — standard Redis URL (every official
    ///   client lib speaks this).
    /// - `tcp://host[:port]` — plain TCP with no leading SELECT round-trip.
    ///
    /// Auth and TLS schemes (`redis://user:pass@…`, `rediss://`) are NOT
    /// supported — kevy itself ships without AUTH/TLS. Including a userinfo
    /// component or using `rediss://` returns [`io::ErrorKind::Unsupported`].
    ///
    /// If a `/db` path segment is present, an explicit `SELECT <db>` is
    /// issued before returning the client. For non-zero indices kevy will
    /// reply with its "only supports DB 0" error and `from_url` propagates
    /// that as [`io::ErrorKind::Other`].
    pub fn from_url(url: &str) -> io::Result<Self> {
        let parsed = parse_url(url)?;
        let mut client = Self::connect(&parsed.host, parsed.port)?;
        if let Some(db) = parsed.db {
            let reply = client.request(&[b"SELECT".to_vec(), db.to_string().into_bytes()])?;
            if let Reply::Error(msg) = reply {
                let text = String::from_utf8_lossy(&msg);
                return Err(io::Error::other(format!("SELECT {db} rejected: {text}")));
            }
        }
        Ok(client)
    }
}

/// Parsed URL pieces. Tiny — full url-rs would be a crates.io dep, against
/// the 0-dep charter. We only need scheme / host / port / db.
#[derive(Debug, PartialEq, Eq)]
struct ParsedUrl {
    host: String,
    port: u16,
    db: Option<u32>,
}

fn parse_url(url: &str) -> io::Result<ParsedUrl> {
    // Scheme split.
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URL missing '://'"))?;
    match scheme {
        "kevy" | "redis" | "tcp" => {}
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

    // Reject userinfo (AUTH) — kevy doesn't support auth.
    if rest.contains('@') {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "userinfo (user:pass@host) is unsupported — kevy has no AUTH",
        ));
    }

    // Split path off authority.
    let (authority, path) = match rest.split_once('/') {
        Some((auth, p)) => (auth, Some(p)),
        None => (rest, None),
    };

    // Host + optional port.
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

    // Optional DB index from path. `tcp://` ignores the path (it's a raw
    // socket URL); `kevy://` and `redis://` honour `/N`.
    let db = match path {
        None | Some("") => None,
        Some(p) if scheme == "tcp" => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("tcp:// URL must not have a path: '/{p}'"),
            ));
        }
        Some(p) => {
            let n: u32 = p.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("bad db index: '{p}' (expected a non-negative integer)"),
                )
            })?;
            Some(n)
        }
    };

    Ok(ParsedUrl { host, port, db })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(u: &str) -> ParsedUrl {
        parse_url(u).unwrap_or_else(|e| panic!("{u}: {e}"))
    }

    #[test]
    fn kevy_redis_tcp_schemes_all_resolve() {
        for url in [
            "kevy://localhost:6379",
            "redis://localhost:6379",
            "tcp://localhost:6379",
        ] {
            let p = parse(url);
            assert_eq!(p.host, "localhost");
            assert_eq!(p.port, 6379);
            assert_eq!(p.db, None);
        }
    }

    #[test]
    fn default_port_is_6379_when_omitted() {
        let p = parse("kevy://example.com");
        assert_eq!(p.host, "example.com");
        assert_eq!(p.port, 6379);
    }

    #[test]
    fn db_path_segment_parsed() {
        assert_eq!(parse("kevy://h:1/0").db, Some(0));
        assert_eq!(parse("redis://h:1/3").db, Some(3));
        assert_eq!(parse("kevy://h").db, None);
        assert_eq!(parse("kevy://h/").db, None);
    }

    #[test]
    fn tls_schemes_rejected() {
        let err = parse_url("rediss://h:6379").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        let err = parse_url("kevys://h:6379").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn auth_userinfo_rejected() {
        let err = parse_url("kevy://user:pass@h:6379").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }

    #[test]
    fn unknown_scheme_rejected() {
        let err = parse_url("memcached://h:11211").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn missing_scheme_rejected() {
        assert!(parse_url("localhost:6379").is_err());
    }

    #[test]
    fn tcp_with_path_rejected() {
        // tcp:// is the raw form — db indices only make sense with the
        // redis/kevy semantic schemes.
        assert!(parse_url("tcp://h:6379/0").is_err());
    }

    #[test]
    fn bad_port_rejected() {
        assert!(parse_url("kevy://h:notaport").is_err());
        assert!(parse_url("kevy://h:99999").is_err()); // > u16::MAX
    }

    #[test]
    fn bad_db_rejected() {
        assert!(parse_url("kevy://h/abc").is_err());
        assert!(parse_url("kevy://h/-1").is_err());
    }

    #[test]
    fn empty_host_rejected() {
        assert!(parse_url("kevy://:6379").is_err());
    }
}
