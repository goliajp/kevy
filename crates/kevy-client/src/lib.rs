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
//! - `mem://<name>`                 — shared in-process bus keyed by `<name>`
//! - `file:///abs/path` /
//!   `file://./rel/path`            — in-process embedded with persistence
//! - `kevy://host[:port][/db]`      — TCP RESP, kevy-native scheme
//! - `redis://host[:port][/db]`     — TCP RESP, standard Redis URL (alias)
//! - `tcp://host[:port]`            — TCP RESP, raw (no SELECT round-trip)
//!
//! Auth (`redis://user:pass@…`) and TLS (`rediss://`) are rejected up front
//! — kevy ships without either. v1.1.0 added the full string/hash/list/set/
//! zset + one-shot `PUBLISH` surface. v1.2.0 added the pub/sub *consumer*
//! side as a separate [`Subscriber`] type — a subscribed connection cannot
//! send normal commands, so it needs its own socket and lives outside the
//! `Connection` enum. v1.3.0 routes `mem://<name>` / `file:///path` through
//! a process-local registry so the publisher and consumer can find each
//! other when both opens use the same URL. The trait-vs-enum design
//! decision is enum for now (closed two-backend universe); see ROADMAP
//! for the trait extension path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::io;
use std::time::Duration;

use kevy_embedded::Store;
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

mod collections;
mod scan;
mod subscribe;
mod transaction;
mod url;

pub use subscribe::{PubsubEvent, Subscriber, SubscriberEvents, SubscriberMessages};
pub use transaction::{Transaction, TransactionReplies};

pub(crate) use url::{Target, parse_url, resolve_store};

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
    /// See the crate-level docs for the supported URL forms. From v1.3.0,
    /// two `Connection::open` calls with the same `mem://<name>` or
    /// `file:///path` URL share the same backing `Store` — and the same
    /// pub/sub bus, so `Connection::publish` reaches a `Subscriber::open`
    /// opened with the same URL.
    pub fn open(url: &str) -> io::Result<Self> {
        let parsed = parse_url(url)?;
        match parsed {
            Target::Remote(remote_url) => Ok(Self::Remote(RespClient::from_url(&remote_url)?)),
            embed => Ok(Self::Embedded(resolve_store(&embed)?)),
        }
    }

    /// `PING`. Returns `()` on `+PONG`, propagates any IO or RESP error.
    /// The first thing every healthcheck calls.
    pub fn ping(&mut self) -> io::Result<()> {
        match self {
            Self::Embedded(_) => Ok(()),
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

    /// `SET key value PX ttl_ms`. Convenience for the common
    /// "cache with expiry" pattern; equivalent to `set` + `expire` but
    /// atomic.
    pub fn set_with_ttl(&mut self, key: &[u8], value: &[u8], ttl: Duration) -> io::Result<()> {
        match self {
            Self::Embedded(s) => s.set_with_ttl(key, value, ttl).map(|_| ()),
            Self::Remote(c) => {
                let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
                let args = vec![
                    b"SET".to_vec(),
                    key.to_vec(),
                    value.to_vec(),
                    b"PX".to_vec(),
                    ms.to_string().into_bytes(),
                ];
                match c.request(&args)? {
                    Reply::Simple(s) if s == b"OK" => Ok(()),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `MGET key [key ...]` — one reply per key, `None` for missing /
    /// wrong-type. Returns in the same order as `keys`.
    pub fn mget(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Option<Vec<u8>>>> {
        match self {
            Self::Embedded(s) => keys.iter().map(|k| s.get(k)).collect(),
            Self::Remote(c) => {
                let mut args = Vec::with_capacity(keys.len() + 1);
                args.push(b"MGET".to_vec());
                args.extend(keys.iter().map(|k| k.to_vec()));
                match c.request(&args)? {
                    Reply::Array(items) => items
                        .into_iter()
                        .map(|r| match r {
                            Reply::Bulk(v) => Ok(Some(v)),
                            Reply::Nil => Ok(None),
                            other => Err(unexpected(other)),
                        })
                        .collect(),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `MSET key value [key value ...]` — set every pair atomically.
    pub fn mset(&mut self, pairs: &[(&[u8], &[u8])]) -> io::Result<()> {
        match self {
            Self::Embedded(s) => {
                for (k, v) in pairs {
                    s.set(k, v)?;
                }
                Ok(())
            }
            Self::Remote(c) => {
                let mut args = Vec::with_capacity(pairs.len() * 2 + 1);
                args.push(b"MSET".to_vec());
                for (k, v) in pairs {
                    args.push(k.to_vec());
                    args.push(v.to_vec());
                }
                match c.request(&args)? {
                    Reply::Simple(s) if s == b"OK" => Ok(()),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `PUBLISH channel message`. Returns the count of subscribers
    /// that received the message.
    ///
    /// As of v1.3.0, the embedded backend has a real in-process pub/sub
    /// bus: when a [`Subscriber`] is open against the same `mem://<name>`
    /// or `file:///path` URL, this delivers there and returns the actual
    /// receiver count. Anonymous `mem://` keeps the old "no subscribers,
    /// returns 0" behaviour (the URL is its own bus, by design).
    ///
    /// The pub/sub *consumer* side lives in [`Subscriber`]. On the remote
    /// backend a subscribed TCP connection cannot send normal commands
    /// per the RESP spec; the embedded backend has no such restriction
    /// but `Subscriber` is still a distinct type for API symmetry.
    pub fn publish(&mut self, channel: &[u8], message: &[u8]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => Ok(s.publish(channel, message)),
            Self::Remote(c) => match c.request(&vec3(b"PUBLISH", channel, message))? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Crate-internal helpers, used here + by `collections.rs` + `subscribe.rs`.
// ─────────────────────────────────────────────────────────────────────────

pub(crate) fn vec2(verb: &[u8], a: &[u8]) -> Vec<Vec<u8>> {
    vec![verb.to_vec(), a.to_vec()]
}

pub(crate) fn vec3(verb: &[u8], a: &[u8], b: &[u8]) -> Vec<Vec<u8>> {
    vec![verb.to_vec(), a.to_vec(), b.to_vec()]
}

pub(crate) fn string(b: Vec<u8>) -> String {
    String::from_utf8_lossy(&b).into_owned()
}

pub(crate) fn unexpected(r: Reply) -> io::Error {
    let kind = match r {
        Reply::Simple(_) => "simple-string",
        Reply::Error(_) => "error",
        Reply::Int(_) => "integer",
        Reply::Bulk(_) => "bulk-string",
        Reply::Nil | Reply::Null => "nil",
        Reply::Array(_) => "array",
        Reply::Map(_) => "map",
        Reply::Set(_) => "set",
        Reply::Double(_) => "double",
        Reply::Boolean(_) => "boolean",
        Reply::Verbatim { .. } => "verbatim-string",
        Reply::BigNumber(_) => "big-number",
        Reply::Push(_) => "push",
        Reply::BlobError(_) => "blob-error",
    };
    io::Error::other(format!("unexpected RESP reply variant: {kind}"))
}

pub(crate) fn array_to_bulks(items: Vec<Reply>) -> io::Result<Vec<Vec<u8>>> {
    items
        .into_iter()
        .map(|r| match r {
            Reply::Bulk(v) => Ok(v),
            Reply::Simple(v) => Ok(v),
            Reply::Nil => Ok(Vec::new()),
            other => Err(unexpected(other)),
        })
        .collect()
}

pub(crate) fn store_err(e: kevy_embedded::StoreError) -> io::Error {
    io::Error::other(format!("kevy-store: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke against the embedded backend: every generic + string method
    /// delegating to `Store`. Per-collection coverage (hash/list/set/zset)
    /// lives in `collections::tests`.
    #[test]
    fn embedded_mem_full_crud_round_trip() {
        let mut c = Connection::open("mem://").unwrap();
        c.ping().unwrap();

        c.set(b"k", b"v").unwrap();
        assert_eq!(c.get(b"k").unwrap(), Some(b"v".to_vec()));

        assert_eq!(c.del(&[&b"k"[..], &b"missing"[..]]).unwrap(), 1);
        assert_eq!(c.get(b"k").unwrap(), None);

        c.set(b"a", b"1").unwrap();
        c.set(b"b", b"2").unwrap();
        assert_eq!(c.exists(&[&b"a"[..], &b"b"[..], &b"none"[..]]).unwrap(), 2);

        assert_eq!(c.incr(b"counter").unwrap(), 1);
        assert_eq!(c.incr_by(b"counter", 9).unwrap(), 10);

        c.set(b"timed", b"x").unwrap();
        assert!(c.expire(b"timed", Duration::from_secs(60)).unwrap());
        let ttl = c.ttl_ms(b"timed").unwrap();
        assert!((0..=60_000).contains(&ttl), "ttl_ms = {ttl}");
        assert!(c.persist(b"timed").unwrap());
        assert_eq!(c.ttl_ms(b"timed").unwrap(), -1);

        assert_eq!(c.type_of(b"none").unwrap(), "none");
        assert_eq!(c.type_of(b"timed").unwrap(), "string");

        assert!(c.dbsize().unwrap() >= 3);
        c.flush().unwrap();
        assert_eq!(c.dbsize().unwrap(), 0);

        c.set_with_ttl(b"timed2", b"x", Duration::from_secs(60))
            .unwrap();
        let ttl = c.ttl_ms(b"timed2").unwrap();
        assert!((0..=60_000).contains(&ttl));
    }

    #[test]
    fn anonymous_mem_publish_returns_zero() {
        // No bus, no subscribers — by design.
        let mut c = Connection::open("mem://").unwrap();
        assert_eq!(c.publish(b"chan", b"hi").unwrap(), 0);
    }

    #[test]
    fn embedded_mget_mset() {
        let mut c = Connection::open("mem://").unwrap();
        c.mset(&[
            (b"a".as_ref(), b"1".as_ref()),
            (b"b".as_ref(), b"2".as_ref()),
        ])
        .unwrap();
        let got = c.mget(&[&b"a"[..], &b"b"[..], &b"missing"[..]]).unwrap();
        assert_eq!(
            got,
            vec![Some(b"1".to_vec()), Some(b"2".to_vec()), None]
        );
    }

    #[test]
    fn embedded_multi_rejected_unsupported() {
        let mut c = Connection::open("mem://").unwrap();
        let err = c.multi().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
