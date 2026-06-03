//! kevy-client — unified KV facade so downstream code can switch between
//! in-process embedded and TCP-server backends with one URL string.
//!
//! ```no_run
//! use kevy_client::Connection;
//!
//! // Same business code regardless of backend:
//! let mut conn = Connection::open(std::env::var("MY_KEVY_URL").unwrap().as_str())?;
//! conn.set(b"hello", b"world")?;
//! assert_eq!(conn.get(b"hello")?, Some(b"world".to_vec()));
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! URL schemes:
//! - `mem://`                       — in-process embedded, in-memory only
//! - `file:///abs/path` /
//!   `file://./rel/path`            — in-process embedded with persistence
//! - `kevy://host[:port][/db]`      — TCP RESP, kevy-native scheme
//! - `redis://host[:port][/db]`     — TCP RESP, standard Redis URL (alias)
//! - `tcp://host[:port]`            — TCP RESP, raw (no SELECT round-trip)
//!
//! Auth (`redis://user:pass@…`) and TLS (`rediss://`) are rejected up front
//! — kevy ships without either. v1.0.5 covers the string + generic-key
//! command subset (the 80% of cache use); hash/list/set/zset/pubsub are on
//! the v1.1.0 backlog. The trait-vs-enum design decision is enum for now
//! (closed two-backend universe); see ROADMAP for the trait extension path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use kevy_embedded::{Config, Store};
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

/// One open connection to a kevy backend, opaque about whether the backend
/// is in-process or over TCP.
pub enum Connection {
    /// In-process [`kevy_embedded::Store`].
    Embedded(Store),
    /// TCP [`kevy_resp_client::RespClient`].
    Remote(RespClient),
}

impl Connection {
    /// Open a backend chosen by URL scheme.
    ///
    /// See the crate-level docs for the supported URL forms.
    pub fn open(url: &str) -> io::Result<Self> {
        let parsed = parse_url(url)?;
        match parsed {
            Target::EmbedMemory => Ok(Self::Embedded(Store::open(Config::default())?)),
            Target::EmbedPersist(path) => Ok(Self::Embedded(Store::open(
                Config::default().with_persist(path),
            )?)),
            // RespClient::from_url already speaks kevy:// + redis:// + tcp://
            // (added in v1.0.3) — delegate to it for the network targets so
            // the URL parser stays in one place.
            Target::Remote(url) => Ok(Self::Remote(RespClient::from_url(&url)?)),
        }
    }

    /// `PING`. Returns `()` on `+PONG`, propagates any IO or RESP error.
    /// The first thing every healthcheck calls.
    pub fn ping(&mut self) -> io::Result<()> {
        match self {
            Self::Embedded(_) => Ok(()), // embedded is alive iff this method is called
            Self::Remote(c) => match c.request(&[b"PING".to_vec()])? {
                Reply::Simple(s) if s == b"PONG" => Ok(()),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `SET key value`. Unconditional set (no NX/XX). Returns `()` on success.
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        match self {
            Self::Embedded(s) => s.set(key, value).map(|_| ()),
            Self::Remote(c) => match c.request(&vec3(b"SET", key, value))? {
                Reply::Simple(s) if s == b"OK" => Ok(()),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `GET key`. `None` if absent or expired.
    pub fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s.get(key),
            Self::Remote(c) => match c.request(&vec2(b"GET", key))? {
                Reply::Bulk(v) => Ok(Some(v)),
                Reply::Nil => Ok(None),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `DEL key [key ...]`. Returns the count of keys that were actually
    /// removed (existing + dropped). Missing keys don't contribute.
    pub fn del(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.del(keys),
            Self::Remote(c) => {
                let mut args = Vec::with_capacity(keys.len() + 1);
                args.push(b"DEL".to_vec());
                args.extend(keys.iter().map(|k| k.to_vec()));
                match c.request(&args)? {
                    Reply::Int(n) if n >= 0 => Ok(n as usize),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `EXISTS key [key ...]`. Count of keys present (a single key can
    /// contribute >1 if passed multiple times, matching Redis semantics).
    pub fn exists(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.exists(keys),
            Self::Remote(c) => {
                let mut args = Vec::with_capacity(keys.len() + 1);
                args.push(b"EXISTS".to_vec());
                args.extend(keys.iter().map(|k| k.to_vec()));
                match c.request(&args)? {
                    Reply::Int(n) if n >= 0 => Ok(n as usize),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `INCR key`. Returns the post-increment value. Errors on non-integer
    /// stored value.
    pub fn incr(&mut self, key: &[u8]) -> io::Result<i64> {
        match self {
            Self::Embedded(s) => s.incr(key),
            Self::Remote(c) => match c.request(&vec2(b"INCR", key))? {
                Reply::Int(n) => Ok(n),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `INCRBY key delta`. Negative delta is `DECRBY`. Returns post-value.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> io::Result<i64> {
        match self {
            Self::Embedded(s) => s.incr_by(key, delta),
            Self::Remote(c) => {
                let args = vec![
                    b"INCRBY".to_vec(),
                    key.to_vec(),
                    delta.to_string().into_bytes(),
                ];
                match c.request(&args)? {
                    Reply::Int(n) => Ok(n),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `PEXPIRE key ttl_ms`. Returns whether the key existed and got a TTL.
    pub fn expire(&mut self, key: &[u8], ttl: Duration) -> io::Result<bool> {
        match self {
            Self::Embedded(s) => s.expire(key, ttl),
            Self::Remote(c) => {
                let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
                let args = vec![b"PEXPIRE".to_vec(), key.to_vec(), ms.to_string().into_bytes()];
                match c.request(&args)? {
                    Reply::Int(1) => Ok(true),
                    Reply::Int(0) => Ok(false),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `PERSIST key`. Returns whether a TTL was actually removed.
    pub fn persist(&mut self, key: &[u8]) -> io::Result<bool> {
        match self {
            Self::Embedded(s) => s.persist(key),
            Self::Remote(c) => match c.request(&vec2(b"PERSIST", key))? {
                Reply::Int(1) => Ok(true),
                Reply::Int(0) => Ok(false),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `PTTL key`. Returns ms remaining, -2 if no key, -1 if key has no TTL.
    pub fn ttl_ms(&mut self, key: &[u8]) -> io::Result<i64> {
        match self {
            Self::Embedded(s) => Ok(s.ttl_ms(key)),
            Self::Remote(c) => match c.request(&vec2(b"PTTL", key))? {
                Reply::Int(n) => Ok(n),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `TYPE key`. Returns the value's type as a Redis-style string (e.g.
    /// `"string"`, `"hash"`, `"list"`, `"set"`, `"zset"`, or `"none"` if
    /// the key doesn't exist).
    pub fn type_of(&mut self, key: &[u8]) -> io::Result<String> {
        match self {
            Self::Embedded(s) => Ok(s.type_of(key).to_string()),
            Self::Remote(c) => match c.request(&vec2(b"TYPE", key))? {
                Reply::Simple(s) => Ok(string(s)),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `DBSIZE`. Total live keys at the time of the call.
    pub fn dbsize(&mut self) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => Ok(s.dbsize()),
            Self::Remote(c) => match c.request(&[b"DBSIZE".to_vec()])? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `FLUSHDB`. Drops every key. Persistence remains opted-in; embedded
    /// `with_persist` will rewrite the AOF on its next sync cycle.
    pub fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Embedded(s) => s.flush(),
            Self::Remote(c) => match c.request(&[b"FLUSHDB".to_vec()])? {
                Reply::Simple(s) if s == b"OK" => Ok(()),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }
}

// =====================================================================
// URL parsing
// =====================================================================

/// What `parse_url` resolves an input to.
#[derive(Debug)]
enum Target {
    /// `mem://` — in-process, in-memory only.
    EmbedMemory,
    /// `file://path` — in-process with persistence in `path`.
    EmbedPersist(PathBuf),
    /// `kevy://…` / `redis://…` / `tcp://…` — delegate to RespClient.
    Remote(String),
}

fn parse_url(url: &str) -> io::Result<Target> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "URL missing '://'"))?;
    match scheme {
        "mem" => {
            if !rest.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "mem:// URL must be empty after the scheme (e.g. `mem://`)",
                ));
            }
            Ok(Target::EmbedMemory)
        }
        "file" => {
            // file:///abs/path → "/abs/path"; file://./rel → "./rel".
            // The triple-slash form is the standard file:// URI for an
            // absolute path; we treat any leading `/` as part of the path.
            let path = rest;
            if path.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file:// URL must include a path (e.g. `file:///var/lib/myapp`)",
                ));
            }
            Ok(Target::EmbedPersist(PathBuf::from(path)))
        }
        "kevy" | "redis" | "tcp" => Ok(Target::Remote(url.to_string())),
        "rediss" | "kevys" => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TLS schemes (rediss://, kevys://) are unsupported — kevy has no TLS",
        )),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown URL scheme '{other}://'"),
        )),
    }
}

// =====================================================================
// Small RESP helpers
// =====================================================================

fn vec2(verb: &[u8], a: &[u8]) -> Vec<Vec<u8>> {
    vec![verb.to_vec(), a.to_vec()]
}

fn vec3(verb: &[u8], a: &[u8], b: &[u8]) -> Vec<Vec<u8>> {
    vec![verb.to_vec(), a.to_vec(), b.to_vec()]
}

fn string(b: Vec<u8>) -> String {
    String::from_utf8_lossy(&b).into_owned()
}

fn unexpected(r: Reply) -> io::Error {
    let kind = match r {
        Reply::Simple(_) => "simple-string",
        Reply::Error(_) => "error",
        Reply::Int(_) => "integer",
        Reply::Bulk(_) => "bulk-string",
        Reply::Nil => "nil",
        Reply::Array(_) => "array",
    };
    io::Error::other(format!("unexpected RESP reply variant: {kind}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mem_url() {
        assert!(matches!(parse_url("mem://").unwrap(), Target::EmbedMemory));
        assert!(parse_url("mem://something").is_err());
    }

    #[test]
    fn parse_file_url() {
        match parse_url("file:///var/lib/myapp").unwrap() {
            Target::EmbedPersist(p) => assert_eq!(p, PathBuf::from("/var/lib/myapp")),
            _ => panic!("wrong variant"),
        }
        match parse_url("file://./data").unwrap() {
            Target::EmbedPersist(p) => assert_eq!(p, PathBuf::from("./data")),
            _ => panic!("wrong variant"),
        }
        assert!(parse_url("file://").is_err());
    }

    #[test]
    fn parse_remote_urls_delegate() {
        for url in ["kevy://h:6379", "redis://h:6379/0", "tcp://h:6379"] {
            match parse_url(url).unwrap() {
                Target::Remote(u) => assert_eq!(u, url),
                _ => panic!("wrong variant"),
            }
        }
    }

    #[test]
    fn parse_tls_rejected() {
        assert_eq!(
            parse_url("rediss://h:6379").unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn parse_unknown_scheme_rejected() {
        assert_eq!(
            parse_url("memcached://h:11211").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    // Functional smoke against the embedded backend — covers every method
    // delegating to Store. (Remote backend smoke needs a running server;
    // see crates/kevy/tests/ for that integration in the next pass.)
    #[test]
    fn embedded_mem_full_crud_round_trip() {
        let mut c = Connection::open("mem://").unwrap();
        c.ping().unwrap();

        c.set(b"k", b"v").unwrap();
        assert_eq!(c.get(b"k").unwrap(), Some(b"v".to_vec()));

        // del returns 1 (existing), 0 (missing).
        assert_eq!(c.del(&[&b"k"[..], &b"missing"[..]]).unwrap(), 1);
        assert_eq!(c.get(b"k").unwrap(), None);

        // exists counts each present.
        c.set(b"a", b"1").unwrap();
        c.set(b"b", b"2").unwrap();
        assert_eq!(c.exists(&[&b"a"[..], &b"b"[..], &b"none"[..]]).unwrap(), 2);

        // incr / incr_by — fresh counter starts at 0.
        assert_eq!(c.incr(b"counter").unwrap(), 1);
        assert_eq!(c.incr_by(b"counter", 9).unwrap(), 10);

        // expire + ttl_ms + persist.
        c.set(b"timed", b"x").unwrap();
        assert!(c.expire(b"timed", Duration::from_secs(60)).unwrap());
        let ttl = c.ttl_ms(b"timed").unwrap();
        assert!((0..=60_000).contains(&ttl), "ttl_ms = {ttl}");
        assert!(c.persist(b"timed").unwrap());
        // After persist, no expiry → ttl_ms is -1.
        assert_eq!(c.ttl_ms(b"timed").unwrap(), -1);

        // type_of for absent + present.
        assert_eq!(c.type_of(b"none").unwrap(), "none");
        assert_eq!(c.type_of(b"timed").unwrap(), "string");

        // dbsize / flush.
        assert!(c.dbsize().unwrap() >= 3);
        c.flush().unwrap();
        assert_eq!(c.dbsize().unwrap(), 0);
    }
}
