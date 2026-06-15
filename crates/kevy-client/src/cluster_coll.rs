//! [`ClusterClient`] collection commands (hash / list / set / sorted set).
//! Every collection command is single-key, so each routes to its key's owner
//! shard — split out of `cluster.rs` for the 500-LOC house rule. The multi-key
//! set-combine ops (SINTER/SUNION/SDIFF) route by their first key: like Redis
//! Cluster, they require all keys in one slot (use a `{hashtag}`), else the
//! server answers `-MOVED`.

use std::io;

use kevy_resp::Reply;

use crate::cluster::ClusterClient;
use crate::collections::{list_pop, list_push, remote_set_combine, set_multi};
use crate::{array_to_bulks, string, unexpected, vec2, vec3};

/// `match reply` → non-negative count, shared by the many `*LEN`/`*ADD`/`*REM`
/// ops that reply `:N`.
fn count(reply: Reply) -> io::Result<usize> {
    match reply {
        Reply::Int(n) if n >= 0 => Ok(n as usize),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

/// `match reply` → bulk array (HGETALL/SMEMBERS/LRANGE/…).
fn bulks(reply: Reply) -> io::Result<Vec<Vec<u8>>> {
    match reply {
        Reply::Array(items) => array_to_bulks(items),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

impl ClusterClient {
    // ===== Hash =====

    /// `HSET key field value [field value ...]` — count of newly added fields.
    pub fn hset(&mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> io::Result<usize> {
        let mut args = Vec::with_capacity(2 + pairs.len() * 2);
        args.push(b"HSET".to_vec());
        args.push(key.to_vec());
        for (f, v) in pairs {
            args.push(f.to_vec());
            args.push(v.to_vec());
        }
        count(self.request_keyed(key, &args)?)
    }

    /// `HGET key field`. `None` when key or field absent.
    pub fn hget(&mut self, key: &[u8], field: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.request_keyed(key, &vec3(b"HGET", key, field))? {
            Reply::Bulk(v) => Ok(Some(v)),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HDEL key field [field ...]` — count of fields removed.
    pub fn hdel(&mut self, key: &[u8], fields: &[&[u8]]) -> io::Result<usize> {
        let mut args = Vec::with_capacity(fields.len() + 2);
        args.push(b"HDEL".to_vec());
        args.push(key.to_vec());
        args.extend(fields.iter().map(|f| f.to_vec()));
        count(self.request_keyed(key, &args)?)
    }

    /// `HLEN key`.
    pub fn hlen(&mut self, key: &[u8]) -> io::Result<usize> {
        count(self.request_keyed(key, &vec2(b"HLEN", key))?)
    }

    /// `HGETALL key` — flat `[f0, v0, f1, v1, …]`.
    pub fn hgetall(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        bulks(self.request_keyed(key, &vec2(b"HGETALL", key))?)
    }

    /// `HKEYS key`.
    pub fn hkeys(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        bulks(self.request_keyed(key, &vec2(b"HKEYS", key))?)
    }

    /// `HVALS key`.
    pub fn hvals(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        bulks(self.request_keyed(key, &vec2(b"HVALS", key))?)
    }

    // ===== List =====

    /// `LPUSH key value [value ...]` — new list length.
    pub fn lpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        list_push(self.route_mut(key), b"LPUSH", key, values)
    }

    /// `RPUSH key value [value ...]` — new list length.
    pub fn rpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        list_push(self.route_mut(key), b"RPUSH", key, values)
    }

    /// `LPOP key count`.
    pub fn lpop(&mut self, key: &[u8], n: usize) -> io::Result<Vec<Vec<u8>>> {
        list_pop(self.route_mut(key), b"LPOP", key, n)
    }

    /// `RPOP key count`.
    pub fn rpop(&mut self, key: &[u8], n: usize) -> io::Result<Vec<Vec<u8>>> {
        list_pop(self.route_mut(key), b"RPOP", key, n)
    }

    /// `LLEN key`.
    pub fn llen(&mut self, key: &[u8]) -> io::Result<usize> {
        count(self.request_keyed(key, &vec2(b"LLEN", key))?)
    }

    /// `LRANGE key start stop` (Redis-style negative indices).
    pub fn lrange(&mut self, key: &[u8], start: i64, stop: i64) -> io::Result<Vec<Vec<u8>>> {
        let args = vec![
            b"LRANGE".to_vec(),
            key.to_vec(),
            start.to_string().into_bytes(),
            stop.to_string().into_bytes(),
        ];
        bulks(self.request_keyed(key, &args)?)
    }

    // ===== Set =====

    /// `SADD key member [member ...]` — count newly added.
    pub fn sadd(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        set_multi(self.route_mut(key), b"SADD", key, members)
    }

    /// `SREM key member [member ...]` — count removed.
    pub fn srem(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        set_multi(self.route_mut(key), b"SREM", key, members)
    }

    /// `SMEMBERS key`.
    pub fn smembers(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        bulks(self.request_keyed(key, &vec2(b"SMEMBERS", key))?)
    }

    /// `SCARD key`.
    pub fn scard(&mut self, key: &[u8]) -> io::Result<usize> {
        count(self.request_keyed(key, &vec2(b"SCARD", key))?)
    }

    /// `SISMEMBER key member`.
    pub fn sismember(&mut self, key: &[u8], member: &[u8]) -> io::Result<bool> {
        match self.request_keyed(key, &vec3(b"SISMEMBER", key, member))? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SINTER key [key ...]` — all keys must share a slot (route by the first).
    pub fn sinter(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        self.set_combine(b"SINTER", keys)
    }

    /// `SUNION key [key ...]` — same-slot.
    pub fn sunion(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        self.set_combine(b"SUNION", keys)
    }

    /// `SDIFF key [key ...]` — same-slot.
    pub fn sdiff(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        self.set_combine(b"SDIFF", keys)
    }

    fn set_combine(&mut self, verb: &[u8], keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        let Some(first) = keys.first() else {
            return Ok(Vec::new());
        };
        remote_set_combine(self.route_mut(first), verb, keys)
    }

    // ===== Sorted set =====

    /// `ZADD key score member [score member ...]` — count newly added.
    pub fn zadd(&mut self, key: &[u8], pairs: &[(f64, &[u8])]) -> io::Result<usize> {
        let mut args = Vec::with_capacity(2 + pairs.len() * 2);
        args.push(b"ZADD".to_vec());
        args.push(key.to_vec());
        for (score, m) in pairs {
            args.push(score.to_string().into_bytes());
            args.push(m.to_vec());
        }
        count(self.request_keyed(key, &args)?)
    }

    /// `ZREM key member [member ...]` — count removed.
    pub fn zrem(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        set_multi(self.route_mut(key), b"ZREM", key, members)
    }

    /// `ZSCORE key member`. `None` if absent.
    pub fn zscore(&mut self, key: &[u8], member: &[u8]) -> io::Result<Option<f64>> {
        match self.request_keyed(key, &vec3(b"ZSCORE", key, member))? {
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
        }
    }

    /// `ZCARD key`.
    pub fn zcard(&mut self, key: &[u8]) -> io::Result<usize> {
        count(self.request_keyed(key, &vec2(b"ZCARD", key))?)
    }

    /// `ZRANGE key start stop` (ascending score; negative indices from tail).
    pub fn zrange(&mut self, key: &[u8], start: i64, stop: i64) -> io::Result<Vec<Vec<u8>>> {
        let args = vec![
            b"ZRANGE".to_vec(),
            key.to_vec(),
            start.to_string().into_bytes(),
            stop.to_string().into_bytes(),
        ];
        bulks(self.request_keyed(key, &args)?)
    }
}
