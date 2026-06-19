//! Async mirror of the string + generic key commands on
//! [`kevy_client::Connection`]. Each method here is a 1:1 translation
//! of the corresponding blocking method: same name, same arguments,
//! same return type modulo `.await`.

use std::io;
use std::time::Duration;

use kevy_resp::Reply;

use crate::conn::AsyncConnection;
use crate::reply::{string, unexpected, vec2, vec3};

impl AsyncConnection {
    /// `SET key value`. Unconditional set; returns on `+OK`.
    pub async fn set(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        match self.codec_mut().request(&vec3(b"SET", key, value)).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `GET key`. `None` if absent or expired.
    pub async fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.codec_mut().request(&vec2(b"GET", key)).await? {
            Reply::Bulk(v) => Ok(Some(v)),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `DEL key [key ...]`. Returns the count actually removed.
    pub async fn del(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        let mut args = Vec::with_capacity(keys.len() + 1);
        args.push(b"DEL".to_vec());
        args.extend(keys.iter().map(|k| k.to_vec()));
        match self.codec_mut().request(&args).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `EXISTS key [key ...]`. Count of keys present (a key passed N
    /// times counts N if it exists).
    pub async fn exists(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        let mut args = Vec::with_capacity(keys.len() + 1);
        args.push(b"EXISTS".to_vec());
        args.extend(keys.iter().map(|k| k.to_vec()));
        match self.codec_mut().request(&args).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCR key`. Returns post-increment value.
    pub async fn incr(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.codec_mut().request(&vec2(b"INCR", key)).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCRBY key delta`. Negative delta = `DECRBY`.
    pub async fn incr_by(&mut self, key: &[u8], delta: i64) -> io::Result<i64> {
        let args = vec![
            b"INCRBY".to_vec(),
            key.to_vec(),
            delta.to_string().into_bytes(),
        ];
        match self.codec_mut().request(&args).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PEXPIRE key ttl_ms`. Returns whether the key existed and got
    /// a TTL set.
    pub async fn expire(&mut self, key: &[u8], ttl: Duration) -> io::Result<bool> {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        let args = vec![b"PEXPIRE".to_vec(), key.to_vec(), ms.to_string().into_bytes()];
        match self.codec_mut().request(&args).await? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PERSIST key`. Returns whether a TTL was removed.
    pub async fn persist(&mut self, key: &[u8]) -> io::Result<bool> {
        match self.codec_mut().request(&vec2(b"PERSIST", key)).await? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PTTL key`. Ms remaining, -2 if no key, -1 if no TTL.
    pub async fn ttl_ms(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.codec_mut().request(&vec2(b"PTTL", key)).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `TYPE key`. Returns Redis-style type name (`"string"`, `"hash"`,
    /// `"list"`, `"set"`, `"zset"`, or `"none"`).
    pub async fn type_of(&mut self, key: &[u8]) -> io::Result<String> {
        match self.codec_mut().request(&vec2(b"TYPE", key)).await? {
            Reply::Simple(s) => Ok(string(s)),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `DBSIZE`. Total live keys at call time.
    pub async fn dbsize(&mut self) -> io::Result<usize> {
        match self.codec_mut().request(&[b"DBSIZE".to_vec()]).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `FLUSHALL`. WIPES the store. Named `flushall` not `flush` to
    /// avoid colliding with `Write::flush`'s sync-to-disk meaning.
    pub async fn flushall(&mut self) -> io::Result<()> {
        match self.codec_mut().request(&[b"FLUSHALL".to_vec()]).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SET key value PX ttl_ms`. Atomic cache-with-expiry.
    pub async fn set_with_ttl(
        &mut self,
        key: &[u8],
        value: &[u8],
        ttl: Duration,
    ) -> io::Result<()> {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        let args = vec![
            b"SET".to_vec(),
            key.to_vec(),
            value.to_vec(),
            b"PX".to_vec(),
            ms.to_string().into_bytes(),
        ];
        match self.codec_mut().request(&args).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `MGET key [key ...]` — one reply per key, in order.
    pub async fn mget(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Option<Vec<u8>>>> {
        let mut args = Vec::with_capacity(keys.len() + 1);
        args.push(b"MGET".to_vec());
        args.extend(keys.iter().map(|k| k.to_vec()));
        match self.codec_mut().request(&args).await? {
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

    /// `MSET key value [key value ...]` — atomic multi-set.
    pub async fn mset(&mut self, pairs: &[(&[u8], &[u8])]) -> io::Result<()> {
        let mut args = Vec::with_capacity(pairs.len() * 2 + 1);
        args.push(b"MSET".to_vec());
        for (k, v) in pairs {
            args.push(k.to_vec());
            args.push(v.to_vec());
        }
        match self.codec_mut().request(&args).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PUBLISH channel message`. Returns subscriber-receive count.
    pub async fn publish(&mut self, channel: &[u8], message: &[u8]) -> io::Result<usize> {
        match self
            .codec_mut()
            .request(&vec3(b"PUBLISH", channel, message))
            .await?
        {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}
