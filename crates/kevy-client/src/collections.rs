//! Connection methods for the four collection-typed Redis data types:
//! hash, list, set, sorted set. Plus the small `LPUSH`/`SADD`-style
//! request builders shared between them.
//!
//! Lives in its own module so `lib.rs` stays focused on the `Connection`
//! enum + open + the generic + string ops. Behaviour and API are
//! unchanged from the single-file layout in v1.2.0 / v1.3.0.

use std::io;

use kevy_resp::Reply;
use kevy_resp_client::RespClient;

use crate::{Connection, array_to_bulks, store_err, string, unexpected, vec2, vec3};

impl Connection {
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

    /// `SINTER key [key ...]` — intersection of all sets.
    pub fn sinter(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => embed_set_combine(s, keys, SetOp::Inter),
            Self::Remote(c) => remote_set_combine(c, b"SINTER", keys),
        }
    }

    /// `SUNION key [key ...]` — union of all sets.
    pub fn sunion(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => embed_set_combine(s, keys, SetOp::Union),
            Self::Remote(c) => remote_set_combine(c, b"SUNION", keys),
        }
    }

    /// `SDIFF key [key ...]` — members of the first set absent from the rest.
    pub fn sdiff(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        match self {
            Self::Embedded(s) => embed_set_combine(s, keys, SetOp::Diff),
            Self::Remote(c) => remote_set_combine(c, b"SDIFF", keys),
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
}

// ─────────────────────────────────────────────────────────────────────────
// Shared request builders. Both backends accept a slice of byte-slices,
// but the RESP path needs to splat them into a single argv vector.
// ─────────────────────────────────────────────────────────────────────────

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

// Set-combine plumbing: each backend's path computes the intersection /
// union / difference of N sets identified by `keys`.

#[derive(Clone, Copy)]
enum SetOp {
    Inter,
    Union,
    Diff,
}

fn embed_set_combine(
    s: &kevy_embedded::Store,
    keys: &[&[u8]],
    op: SetOp,
) -> io::Result<Vec<Vec<u8>>> {
    use std::collections::HashSet;
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let snapshots: Vec<Vec<Vec<u8>>> = keys
        .iter()
        .map(|k| s.smembers(k))
        .collect::<io::Result<_>>()?;
    let mut iter = snapshots.into_iter();
    let mut acc: HashSet<Vec<u8>> = iter.next().unwrap_or_default().into_iter().collect();
    for rest in iter {
        let other: HashSet<Vec<u8>> = rest.into_iter().collect();
        acc = match op {
            SetOp::Inter => acc.intersection(&other).cloned().collect(),
            SetOp::Union => acc.union(&other).cloned().collect(),
            SetOp::Diff => acc.difference(&other).cloned().collect(),
        };
    }
    Ok(acc.into_iter().collect())
}

fn remote_set_combine(
    c: &mut RespClient,
    verb: &[u8],
    keys: &[&[u8]],
) -> io::Result<Vec<Vec<u8>>> {
    let mut args = Vec::with_capacity(keys.len() + 1);
    args.push(verb.to_vec());
    args.extend(keys.iter().map(|k| k.to_vec()));
    match c.request(&args)? {
        Reply::Array(items) => array_to_bulks(items),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        );
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

        let r = c.zrange(b"lb", 0, -1).unwrap();
        assert_eq!(
            r,
            vec![b"carol".to_vec(), b"alice".to_vec(), b"bob".to_vec()]
        );

        assert_eq!(c.zrem(b"lb", &[&b"carol"[..]]).unwrap(), 1);
        assert_eq!(c.zcard(b"lb").unwrap(), 2);
    }

    #[test]
    fn embedded_set_combine_ops() {
        let mut c = Connection::open("mem://").unwrap();
        c.sadd(b"a", &[&b"x"[..], &b"y"[..], &b"z"[..]]).unwrap();
        c.sadd(b"b", &[&b"y"[..], &b"z"[..], &b"w"[..]]).unwrap();

        let mut inter = c.sinter(&[&b"a"[..], &b"b"[..]]).unwrap();
        inter.sort();
        assert_eq!(inter, vec![b"y".to_vec(), b"z".to_vec()]);

        let mut union = c.sunion(&[&b"a"[..], &b"b"[..]]).unwrap();
        union.sort();
        assert_eq!(
            union,
            vec![b"w".to_vec(), b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]
        );

        let mut diff = c.sdiff(&[&b"a"[..], &b"b"[..]]).unwrap();
        diff.sort();
        assert_eq!(diff, vec![b"x".to_vec()]);

        // Empty input → empty output (no panic).
        assert!(c.sinter(&[]).unwrap().is_empty());
    }
}
