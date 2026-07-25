//! kevy-client — unified KV facade so downstream code can switch between
//! in-process embedded and TCP-server backends with one URL string.
//!
//! ```no_run
//! use kevy_client::Connection;
//!
//! // Same business code regardless of backend:
//! let mut conn = Connection::connect(std::env::var("MY_KEVY_URL").unwrap().as_str())?;
//! conn.set(b"hello", b"world")?;
//! assert_eq!(conn.get(b"hello")?, Some(b"world".to_vec()));
//! # Ok::<(), kevy_client::KevyError>(())
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
//! — kevy ships without either. The crate covers the full string/hash/list/
//! set/zset + one-shot `PUBLISH` surface. The pub/sub *consumer* side is a
//! separate [`Subscriber`] type — a subscribed connection cannot send
//! normal commands, so it needs its own socket and lives outside the
//! `Connection` enum. `mem://<name>` / `file:///path` route through a
//! process-local registry so the publisher and consumer can find each
//! other when both opens use the same URL. The trait-vs-enum design
//! decision is enum for now (closed two-backend universe); see ROADMAP
//! for the trait extension path.
//!
//! Beyond that base the wrap surface tracks the full server op surface:
//! blocking pops (`blpop`/`brpop`/`bzpopmin`), hash field-TTL
//! (`hexpire`/`hpexpire`/`hpersist`/`httl`), zset algebra
//! (`zinterstore`/`zunionstore`/`zintercard` + WEIGHTS/AGGREGATE),
//! declarative indexes (`idx_*`), the CDC change feed (`feed_*`), and
//! non-atomic [`Connection::pipeline`] batching. The full
//! **op-family × wrap-status matrix** lives in this crate's README —
//! anything listed there as raw-only is still reachable through
//! [`Connection::pipeline`] / [`Transaction::queue`] argv passthrough.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::time::Duration;

use kevy_embedded::Store;
use kevy_resp_client::RespClient;

mod blocking;
mod cluster;
mod cluster_coll;
mod collections;
mod feed;
mod hash_ttl;
mod index;
mod pipeline;
mod reply;
mod scan;
mod subscribe;
mod subscribe_io;
mod transaction;
mod url;
mod zalgebra;

pub use blocking::ZPopHit;
pub use cluster::ClusterClient;
pub use feed::{FeedBatch, FeedFrame};
pub use index::{IdxInfo, IdxPage, IdxRow, IdxType};
pub use pipeline::PipelineBuf;
pub use subscribe::{PubsubEvent, Subscriber, SubscriberEvents, SubscriberMessages};
pub use transaction::{Transaction, TransactionReplies};

/// Re-exports so downstream code can name the argument/reply/error types
/// of the wraps without adding kevy-embedded / kevy-resp deps.
pub use kevy_embedded::{HExpireCode, HExpireCond, KevyError, KevyResult, StoreError, ZAggregate};
pub use kevy_resp::Reply;

pub(crate) use reply::{array_to_bulks, num_f64, num_u64, store_err, string, unexpected, vec2, vec3};
pub(crate) use url::{Target, parse_url, resolve_store};

/// One open connection to a kevy backend, opaque about whether the backend
/// is in-process or over TCP.
pub enum Connection {
    /// In-process [`kevy_embedded::Store`]. Boxed because `Store` is
    /// sizeable (carries its `Config`, including the replica
    /// upstream/backoff fields) and dwarfs the `RespClient` variant.
    Embedded(Box<Store>),
    /// TCP [`kevy_resp_client::RespClient`].
    Remote(RespClient),
}

impl Connection {
    /// Connect to a backend chosen by URL scheme.
    ///
    /// See the crate-level docs for the supported URL forms. Two
    /// `Connection::connect` calls with the same `mem://<name>` or
    /// `file:///path` URL share the same backing `Store` — and the same
    /// pub/sub bus, so `Connection::publish` reaches a
    /// `Subscriber::connect_channels` opened with the same URL.
    pub fn connect(url: &str) -> KevyResult<Self> {
        let parsed = parse_url(url)?;
        match parsed {
            Target::Remote(remote_url) => Ok(Self::Remote(RespClient::connect_url(&remote_url)?)),
            embed => Ok(Self::Embedded(Box::new(resolve_store(&embed)?))),
        }
    }

    /// Remote-only feature gate: hand back the RESP client, or explain
    /// why the embedded backend can't serve `feature` (`Unsupported`,
    /// pointing at the `Connection::Embedded` escape hatch).
    pub(crate) fn remote(&mut self, feature: &str) -> KevyResult<&mut RespClient> {
        match self {
            Self::Embedded(_) => Err(KevyError::Unsupported(format!(
                "{feature} is remote-only; on the embedded backend match \
                 Connection::Embedded and use kevy_embedded::Store's typed API"
            ))),
            Self::Remote(c) => Ok(c),
        }
    }

    /// `PING`. Returns `()` on `+PONG`, propagates any IO or RESP error.
    /// The first thing every healthcheck calls.
    pub fn ping(&mut self) -> KevyResult<()> {
        match self {
            Self::Embedded(_) => Ok(()),
            Self::Remote(c) => match c.request_borrowed(&[b"PING"])? {
                Reply::Simple(s) if s == b"PONG" => Ok(()),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `SET key value`. Unconditional set (no NX/XX). Returns `()` on success.
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> KevyResult<()> {
        match self {
            Self::Embedded(s) => s.set(key, value).map(|_| ()),
            Self::Remote(c) => match c.request_borrowed(&[b"SET", key, value])? {
                Reply::Simple(s) if s == b"OK" => Ok(()),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `GET key`. `None` if absent or expired.
    pub fn get(&mut self, key: &[u8]) -> KevyResult<Option<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s.get(key),
            Self::Remote(c) => match c.request_borrowed(&[b"GET", key])? {
                Reply::Bulk(v) => Ok(Some(v)),
                Reply::Nil => Ok(None),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `DEL key [key ...]`. Returns the count of keys that were actually
    /// removed (existing + dropped). Missing keys don't contribute.
    pub fn del(&mut self, keys: &[&[u8]]) -> KevyResult<usize> {
        match self {
            Self::Embedded(s) => s.del(keys),
            Self::Remote(c) => {
                let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
                args.push(b"DEL");
                args.extend_from_slice(keys);
                match c.request_borrowed(&args)? {
                    Reply::Int(n) if n >= 0 => Ok(n as usize),
                    Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `EXISTS key [key ...]`. Count of keys present (a single key can
    /// contribute >1 if passed multiple times, matching Redis semantics).
    pub fn exists(&mut self, keys: &[&[u8]]) -> KevyResult<usize> {
        match self {
            Self::Embedded(s) => s.exists(keys),
            Self::Remote(c) => {
                let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
                args.push(b"EXISTS");
                args.extend_from_slice(keys);
                match c.request_borrowed(&args)? {
                    Reply::Int(n) if n >= 0 => Ok(n as usize),
                    Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `INCR key`. Returns the post-increment value. Errors on non-integer
    /// stored value.
    pub fn incr(&mut self, key: &[u8]) -> KevyResult<i64> {
        match self {
            Self::Embedded(s) => s.incr(key),
            Self::Remote(c) => match c.request_borrowed(&[b"INCR", key])? {
                Reply::Int(n) => Ok(n),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `INCRBY key delta`. Negative delta is `DECRBY`. Returns post-value.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> KevyResult<i64> {
        match self {
            Self::Embedded(s) => s.incr_by(key, delta),
            Self::Remote(c) => {
                let delta_s = delta.to_string();
                match c.request_borrowed(&[b"INCRBY", key, delta_s.as_bytes()])? {
                    Reply::Int(n) => Ok(n),
                    Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `PEXPIRE key ttl_ms`. Returns whether the key existed and got a TTL.
    pub fn expire(&mut self, key: &[u8], ttl: Duration) -> KevyResult<bool> {
        match self {
            Self::Embedded(s) => s.expire(key, ttl),
            Self::Remote(c) => {
                let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
                let ms_s = ms.to_string();
                match c.request_borrowed(&[b"PEXPIRE", key, ms_s.as_bytes()])? {
                    Reply::Int(1) => Ok(true),
                    Reply::Int(0) => Ok(false),
                    Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `PERSIST key`. Returns whether a TTL was actually removed.
    pub fn persist(&mut self, key: &[u8]) -> KevyResult<bool> {
        match self {
            Self::Embedded(s) => s.persist(key),
            Self::Remote(c) => match c.request_borrowed(&[b"PERSIST", key])? {
                Reply::Int(1) => Ok(true),
                Reply::Int(0) => Ok(false),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `PTTL key`. Returns ms remaining, -2 if no key, -1 if key has no TTL.
    pub fn ttl_ms(&mut self, key: &[u8]) -> KevyResult<i64> {
        match self {
            Self::Embedded(s) => Ok(s.ttl_ms(key)),
            Self::Remote(c) => match c.request_borrowed(&[b"PTTL", key])? {
                Reply::Int(n) => Ok(n),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `TYPE key`. Returns the value's type as a Redis-style string (e.g.
    /// `"string"`, `"hash"`, `"list"`, `"set"`, `"zset"`, or `"none"` if
    /// the key doesn't exist).
    pub fn type_of(&mut self, key: &[u8]) -> KevyResult<String> {
        match self {
            Self::Embedded(s) => Ok(s.type_of(key).to_string()),
            Self::Remote(c) => match c.request_borrowed(&[b"TYPE", key])? {
                Reply::Simple(s) => Ok(string(s)),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `DBSIZE`. Total live keys at the time of the call.
    pub fn dbsize(&mut self) -> KevyResult<usize> {
        match self {
            Self::Embedded(s) => Ok(s.dbsize()),
            Self::Remote(c) => match c.request_borrowed(&[b"DBSIZE"])? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `FLUSHALL`. Drops every key. Persistence remains opted-in; embedded
    /// `with_persist` will rewrite the AOF on its next sync cycle.
    ///
    /// Named `flushall` — **not** `flush` — to avoid colliding with
    /// `Write::flush`'s "sync buffered writes to disk" meaning; this WIPES the
    /// store rather than persisting it.
    pub fn flushall(&mut self) -> KevyResult<()> {
        match self {
            Self::Embedded(s) => s.flushall(),
            Self::Remote(c) => match c.request_borrowed(&[b"FLUSHALL"])? {
                Reply::Simple(s) if s == b"OK" => Ok(()),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `SET key value PX ttl_ms`. Convenience for the common
    /// "cache with expiry" pattern; equivalent to `set` + `expire` but
    /// atomic.
    pub fn set_with_ttl(&mut self, key: &[u8], value: &[u8], ttl: Duration) -> KevyResult<()> {
        match self {
            Self::Embedded(s) => s.set_with_ttl(key, value, ttl).map(|_| ()),
            Self::Remote(c) => {
                let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
                let ms_s = ms.to_string();
                match c.request_borrowed(&[b"SET", key, value, b"PX", ms_s.as_bytes()])? {
                    Reply::Simple(s) if s == b"OK" => Ok(()),
                    Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `MGET key [key ...]` — one reply per key, `None` for missing /
    /// wrong-type. Returns in the same order as `keys`.
    pub fn mget(&mut self, keys: &[&[u8]]) -> KevyResult<Vec<Option<Vec<u8>>>> {
        match self {
            Self::Embedded(s) => keys.iter().map(|k| s.get(k)).collect(),
            Self::Remote(c) => {
                let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
                args.push(b"MGET");
                args.extend_from_slice(keys);
                match c.request_borrowed(&args)? {
                    Reply::Array(items) => items
                        .into_iter()
                        .map(|r| match r {
                            Reply::Bulk(v) => Ok(Some(v)),
                            Reply::Nil => Ok(None),
                            other => Err(unexpected(other)),
                        })
                        .collect(),
                    Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `MSET key value [key value ...]` — set every pair atomically.
    pub fn mset(&mut self, pairs: &[(&[u8], &[u8])]) -> KevyResult<()> {
        match self {
            Self::Embedded(s) => {
                for (k, v) in pairs {
                    s.set(k, v)?;
                }
                Ok(())
            }
            Self::Remote(c) => {
                let mut args: Vec<&[u8]> = Vec::with_capacity(pairs.len() * 2 + 1);
                args.push(b"MSET");
                for &(k, v) in pairs {
                    args.push(k);
                    args.push(v);
                }
                match c.request_borrowed(&args)? {
                    Reply::Simple(s) if s == b"OK" => Ok(()),
                    Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `PUBLISH channel message`. Returns the count of subscribers
    /// that received the message.
    ///
    /// The embedded backend has a real in-process pub/sub
    /// bus: when a [`Subscriber`] is open against the same `mem://<name>`
    /// or `file:///path` URL, this delivers there and returns the actual
    /// receiver count. Anonymous `mem://` keeps the old "no subscribers,
    /// returns 0" behaviour (the URL is its own bus, by design).
    ///
    /// The pub/sub *consumer* side lives in [`Subscriber`]. On the remote
    /// backend a subscribed TCP connection cannot send normal commands
    /// per the RESP spec; the embedded backend has no such restriction
    /// but `Subscriber` is still a distinct type for API symmetry.
    pub fn publish(&mut self, channel: &[u8], message: &[u8]) -> KevyResult<usize> {
        match self {
            Self::Embedded(s) => Ok(s.publish(channel, message)),
            Self::Remote(c) => match c.request_borrowed(&[b"PUBLISH", channel, message])? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(KevyError::Protocol(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }
}


#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
