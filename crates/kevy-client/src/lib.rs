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
//! — kevy ships without either. v1.1.0 added the full string/hash/list/set/
//! zset + one-shot `PUBLISH` surface. v1.2.0 added the pub/sub *consumer*
//! side as a separate [`Subscriber`] type — a subscribed connection cannot
//! send normal commands, so it needs its own socket and lives outside the
//! `Connection` enum. The trait-vs-enum design decision is enum for now
//! (closed two-backend universe); see ROADMAP for the trait extension path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use kevy_embedded::{Config, Store};
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

mod subscribe;
pub use subscribe::{PubsubEvent, Subscriber};

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

    // ===== Hash =====

    /// `HSET key field value [field value ...]`. Returns the number of
    /// fields that were newly added (not overwrites).
    pub fn hset(&mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.hset(key, pairs),
            Self::Remote(c) => {
                let mut args = Vec::with_capacity(2 + pairs.len() * 2);
                args.push(b"HSET".to_vec());
                args.push(key.to_vec());
                for (f, v) in pairs {
                    args.push(f.to_vec());
                    args.push(v.to_vec());
                }
                match c.request(&args)? {
                    Reply::Int(n) if n >= 0 => Ok(n as usize),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `HGET key field`. `None` when the key or field is absent.
    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s.hget(key, field),
            Self::Remote(c) => match c.request(&vec3(b"HGET", key, field))? {
                Reply::Bulk(v) => Ok(Some(v)),
                Reply::Nil => Ok(None),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `HDEL key field [field ...]`. Returns the number of fields actually
    /// removed.
    pub fn hdel(&mut self, key: &[u8], fields: &[&[u8]]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.hdel(key, fields),
            Self::Remote(c) => {
                let mut args = Vec::with_capacity(fields.len() + 2);
                args.push(b"HDEL".to_vec());
                args.push(key.to_vec());
                args.extend(fields.iter().map(|f| f.to_vec()));
                match c.request(&args)? {
                    Reply::Int(n) if n >= 0 => Ok(n as usize),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `HLEN key`. Number of fields in the hash (0 if absent).
    pub fn hlen(&mut self, key: &[u8]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.with(|inner| inner.hlen(key)).map_err(store_err),
            Self::Remote(c) => match c.request(&vec2(b"HLEN", key))? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `HGETALL key`. Returns a flat `[f0, v0, f1, v1, ...]` matching the
    /// Redis wire shape; empty when the key is absent.
    pub fn hgetall(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s.with(|inner| inner.hgetall(key)).map_err(store_err),
            Self::Remote(c) => match c.request(&vec2(b"HGETALL", key))? {
                Reply::Array(items) => array_to_bulks(items),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `HKEYS key`. Returns the hash's field names (empty if absent).
    pub fn hkeys(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s.with(|inner| inner.hkeys(key)).map_err(store_err),
            Self::Remote(c) => match c.request(&vec2(b"HKEYS", key))? {
                Reply::Array(items) => array_to_bulks(items),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `HVALS key`. Returns the hash's values (empty if absent).
    pub fn hvals(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s.with(|inner| inner.hvals(key)).map_err(store_err),
            Self::Remote(c) => match c.request(&vec2(b"HVALS", key))? {
                Reply::Array(items) => array_to_bulks(items),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    // ===== List =====

    /// `LPUSH key value [value ...]`. Returns the new list length.
    pub fn lpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.lpush(key, values),
            Self::Remote(c) => list_push(c, b"LPUSH", key, values),
        }
    }

    /// `RPUSH key value [value ...]`. Returns the new list length.
    pub fn rpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.rpush(key, values),
            Self::Remote(c) => list_push(c, b"RPUSH", key, values),
        }
    }

    /// `LPOP key count`. Returns up to `count` values from the head; empty
    /// when the key is absent or already drained.
    pub fn lpop(&mut self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s.lpop(key, count),
            Self::Remote(c) => list_pop(c, b"LPOP", key, count),
        }
    }

    /// `RPOP key count`. Symmetric to [`Self::lpop`] from the tail.
    pub fn rpop(&mut self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s.rpop(key, count),
            Self::Remote(c) => list_pop(c, b"RPOP", key, count),
        }
    }

    /// `LLEN key`. 0 when the key is absent.
    pub fn llen(&mut self, key: &[u8]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.llen(key),
            Self::Remote(c) => match c.request(&vec2(b"LLEN", key))? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `LRANGE key start stop`. Redis-style indexing — negative offsets
    /// count from the tail (`-1` = last element).
    pub fn lrange(&mut self, key: &[u8], start: i64, stop: i64) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s
                .with(|inner| inner.lrange(key, start, stop))
                .map_err(store_err),
            Self::Remote(c) => {
                let args = vec![
                    b"LRANGE".to_vec(),
                    key.to_vec(),
                    start.to_string().into_bytes(),
                    stop.to_string().into_bytes(),
                ];
                match c.request(&args)? {
                    Reply::Array(items) => array_to_bulks(items),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    // ===== Set =====

    /// `SADD key member [member ...]`. Returns count of newly added members.
    pub fn sadd(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.sadd(key, members),
            Self::Remote(c) => set_multi(c, b"SADD", key, members),
        }
    }

    /// `SREM key member [member ...]`. Returns count of removed members.
    pub fn srem(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.srem(key, members),
            Self::Remote(c) => set_multi(c, b"SREM", key, members),
        }
    }

    /// `SMEMBERS key`. Order is implementation-defined; empty if absent.
    pub fn smembers(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s.smembers(key),
            Self::Remote(c) => match c.request(&vec2(b"SMEMBERS", key))? {
                Reply::Array(items) => array_to_bulks(items),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `SCARD key`. 0 when the key is absent.
    pub fn scard(&mut self, key: &[u8]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.scard(key),
            Self::Remote(c) => match c.request(&vec2(b"SCARD", key))? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `SISMEMBER key member`. `false` when key or member absent.
    pub fn sismember(&mut self, key: &[u8], member: &[u8]) -> io::Result<bool> {
        match self {
            Self::Embedded(s) => s
                .with(|inner| inner.sismember(key, member))
                .map_err(store_err),
            Self::Remote(c) => match c.request(&vec3(b"SISMEMBER", key, member))? {
                Reply::Int(1) => Ok(true),
                Reply::Int(0) => Ok(false),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    // ===== Sorted set =====

    /// `ZADD key score member [score member ...]`. Returns count of newly
    /// added members (overwrites don't count).
    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, &[u8])]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.zadd(key, pairs),
            Self::Remote(c) => {
                let mut args = Vec::with_capacity(2 + pairs.len() * 2);
                args.push(b"ZADD".to_vec());
                args.push(key.to_vec());
                for (score, m) in pairs {
                    args.push(score.to_string().into_bytes());
                    args.push(m.to_vec());
                }
                match c.request(&args)? {
                    Reply::Int(n) if n >= 0 => Ok(n as usize),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    /// `ZREM key member [member ...]`. Returns count of removed members.
    pub fn zrem(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.zrem(key, members),
            Self::Remote(c) => set_multi(c, b"ZREM", key, members),
        }
    }

    /// `ZSCORE key member`. `None` if absent.
    pub fn zscore(&mut self, key: &[u8], member: &[u8]) -> io::Result<Option<f64>> {
        match self {
            Self::Embedded(s) => s.zscore(key, member),
            Self::Remote(c) => match c.request(&vec3(b"ZSCORE", key, member))? {
                Reply::Bulk(v) => {
                    let s = std::str::from_utf8(&v)
                        .map_err(|_| io::Error::other("non-utf8 score reply"))?;
                    let n: f64 = s
                        .parse()
                        .map_err(|_| io::Error::other(format!("bad score float: {s}")))?;
                    Ok(Some(n))
                }
                Reply::Nil => Ok(None),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `ZCARD key`. Number of members; 0 if absent.
    pub fn zcard(&mut self, key: &[u8]) -> io::Result<usize> {
        match self {
            Self::Embedded(s) => s.zcard(key),
            Self::Remote(c) => match c.request(&vec2(b"ZCARD", key))? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }

    /// `ZRANGE key start stop`. Ascending-score order; negative indices
    /// count from the tail.
    pub fn zrange(&mut self, key: &[u8], start: i64, stop: i64) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => s
                .with(|inner| inner.zrange(key, start, stop))
                .map(|pairs| pairs.into_iter().map(|(m, _score)| m).collect())
                .map_err(store_err),
            Self::Remote(c) => {
                let args = vec![
                    b"ZRANGE".to_vec(),
                    key.to_vec(),
                    start.to_string().into_bytes(),
                    stop.to_string().into_bytes(),
                ];
                match c.request(&args)? {
                    Reply::Array(items) => array_to_bulks(items),
                    Reply::Error(e) => Err(io::Error::other(string(e))),
                    other => Err(unexpected(other)),
                }
            }
        }
    }

    // ===== Pub/sub =====

    /// `PUBLISH channel message`. Returns the count of subscribers
    /// that received the message.
    ///
    /// On the embedded backend there are no subscribers (single process,
    /// no reactor), so this returns `Ok(0)` — matching Redis's
    /// "publish to a channel with no listeners" semantic.
    ///
    /// The pub/sub *consumer* side lives in [`Subscriber`], which takes
    /// its own dedicated TCP connection: a subscribed connection cannot
    /// send normal commands per the Redis/RESP spec, so it can't share
    /// a socket with this `Connection`.
    pub fn publish(&mut self, channel: &[u8], message: &[u8]) -> io::Result<usize> {
        match self {
            Self::Embedded(_) => Ok(0),
            Self::Remote(c) => match c.request(&vec3(b"PUBLISH", channel, message))? {
                Reply::Int(n) if n >= 0 => Ok(n as usize),
                Reply::Error(e) => Err(io::Error::other(string(e))),
                other => Err(unexpected(other)),
            },
        }
    }
}

// Helpers for the multi-arg list / set commands — both backends accept a
// slice of byte-slices, but the RESP path builds the args vector itself.

fn list_push(
    c: &mut RespClient,
    verb: &[u8],
    key: &[u8],
    values: &[&[u8]],
) -> io::Result<usize> {
    let mut args = Vec::with_capacity(values.len() + 2);
    args.push(verb.to_vec());
    args.push(key.to_vec());
    args.extend(values.iter().map(|v| v.to_vec()));
    match c.request(&args)? {
        Reply::Int(n) if n >= 0 => Ok(n as usize),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

fn list_pop(
    c: &mut RespClient,
    verb: &[u8],
    key: &[u8],
    count: usize,
) -> io::Result<Vec<Vec<u8>>> {
    let args = vec![verb.to_vec(), key.to_vec(), count.to_string().into_bytes()];
    match c.request(&args)? {
        Reply::Array(items) => array_to_bulks(items),
        Reply::Bulk(v) => Ok(vec![v]),
        Reply::Nil => Ok(Vec::new()),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

fn set_multi(
    c: &mut RespClient,
    verb: &[u8],
    key: &[u8],
    members: &[&[u8]],
) -> io::Result<usize> {
    let mut args = Vec::with_capacity(members.len() + 2);
    args.push(verb.to_vec());
    args.push(key.to_vec());
    args.extend(members.iter().map(|m| m.to_vec()));
    match c.request(&args)? {
        Reply::Int(n) if n >= 0 => Ok(n as usize),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

fn array_to_bulks(items: Vec<Reply>) -> io::Result<Vec<Vec<u8>>> {
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

fn store_err(e: kevy_embedded::StoreError) -> io::Error {
    io::Error::other(format!("kevy-store: {e:?}"))
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

        // set_with_ttl — same as set+expire but atomic.
        c.set_with_ttl(b"timed2", b"x", Duration::from_secs(60)).unwrap();
        let ttl = c.ttl_ms(b"timed2").unwrap();
        assert!((0..=60_000).contains(&ttl));
    }

    #[test]
    fn embedded_hash_methods() {
        let mut c = Connection::open("mem://").unwrap();
        let pairs: &[(&[u8], &[u8])] = &[
            (b"name".as_ref(), b"alice".as_ref()),
            (b"age".as_ref(), b"30".as_ref()),
        ];
        assert_eq!(c.hset(b"u:1", pairs).unwrap(), 2);
        assert_eq!(c.hget(b"u:1", b"name").unwrap(), Some(b"alice".to_vec()));
        assert_eq!(c.hget(b"u:1", b"missing").unwrap(), None);
        assert_eq!(c.hlen(b"u:1").unwrap(), 2);

        // hgetall returns flat [f0,v0,f1,v1,...] — sort to make assertion stable.
        let mut all = c.hgetall(b"u:1").unwrap();
        all.sort();
        assert!(all.contains(&b"alice".to_vec()));
        assert!(all.contains(&b"name".to_vec()));

        let mut keys = c.hkeys(b"u:1").unwrap();
        keys.sort();
        assert_eq!(keys, vec![b"age".to_vec(), b"name".to_vec()]);

        let mut vals = c.hvals(b"u:1").unwrap();
        vals.sort();
        assert_eq!(vals, vec![b"30".to_vec(), b"alice".to_vec()]);

        assert_eq!(c.hdel(b"u:1", &[&b"age"[..], &b"missing"[..]]).unwrap(), 1);
        assert_eq!(c.hlen(b"u:1").unwrap(), 1);
    }

    #[test]
    fn embedded_list_methods() {
        let mut c = Connection::open("mem://").unwrap();
        assert_eq!(c.rpush(b"q", &[&b"a"[..], &b"b"[..], &b"c"[..]]).unwrap(), 3);
        assert_eq!(c.lpush(b"q", &[&b"z"[..]]).unwrap(), 4);
        assert_eq!(c.llen(b"q").unwrap(), 4);

        // q = [z, a, b, c]
        assert_eq!(
            c.lrange(b"q", 0, -1).unwrap(),
            vec![b"z".to_vec(), b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );

        assert_eq!(c.lpop(b"q", 2).unwrap(), vec![b"z".to_vec(), b"a".to_vec()]);
        assert_eq!(c.rpop(b"q", 1).unwrap(), vec![b"c".to_vec()]);
        assert_eq!(c.llen(b"q").unwrap(), 1);
    }

    #[test]
    fn embedded_set_methods() {
        let mut c = Connection::open("mem://").unwrap();
        assert_eq!(
            c.sadd(b"s", &[&b"a"[..], &b"b"[..], &b"a"[..]]).unwrap(),
            2
        ); // dedupe
        assert_eq!(c.scard(b"s").unwrap(), 2);
        assert!(c.sismember(b"s", b"a").unwrap());
        assert!(!c.sismember(b"s", b"missing").unwrap());

        let mut m = c.smembers(b"s").unwrap();
        m.sort();
        assert_eq!(m, vec![b"a".to_vec(), b"b".to_vec()]);

        assert_eq!(c.srem(b"s", &[&b"a"[..]]).unwrap(), 1);
        assert_eq!(c.scard(b"s").unwrap(), 1);
    }

    #[test]
    fn embedded_zset_methods() {
        let mut c = Connection::open("mem://").unwrap();
        let pairs: &[(f64, &[u8])] = &[
            (100.0, b"alice".as_ref()),
            (200.0, b"bob".as_ref()),
            (50.0, b"carol".as_ref()),
        ];
        assert_eq!(c.zadd(b"lb", pairs).unwrap(), 3);
        assert_eq!(c.zscore(b"lb", b"bob").unwrap(), Some(200.0));
        assert_eq!(c.zscore(b"lb", b"none").unwrap(), None);
        assert_eq!(c.zcard(b"lb").unwrap(), 3);

        // ZRANGE 0 -1 → ascending by score: carol, alice, bob.
        let r = c.zrange(b"lb", 0, -1).unwrap();
        assert_eq!(
            r,
            vec![b"carol".to_vec(), b"alice".to_vec(), b"bob".to_vec()]
        );

        assert_eq!(c.zrem(b"lb", &[&b"carol"[..]]).unwrap(), 1);
        assert_eq!(c.zcard(b"lb").unwrap(), 2);
    }

    #[test]
    fn embedded_publish_returns_zero() {
        // Single-process embed has no subscribers — semantic match for
        // "PUBLISH to a channel nobody listens to".
        let mut c = Connection::open("mem://").unwrap();
        assert_eq!(c.publish(b"chan", b"hi").unwrap(), 0);
    }
}
