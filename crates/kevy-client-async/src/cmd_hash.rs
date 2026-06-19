//! Async mirror of hash commands on `kevy_client::Connection`.

use std::io;

use kevy_resp::Reply;

use crate::conn::AsyncConnection;
use crate::reply::{array_to_bulks, string, unexpected, vec2, vec3};

impl AsyncConnection {
    /// `HSET key field value [field value ...]`. Returns count of
    /// fields newly created (overwrites don't count).
    pub async fn hset(
        &mut self,
        key: &[u8],
        pairs: &[(&[u8], &[u8])],
    ) -> io::Result<usize> {
        let mut args = Vec::with_capacity(2 + pairs.len() * 2);
        args.push(b"HSET".to_vec());
        args.push(key.to_vec());
        for (f, v) in pairs {
            args.push(f.to_vec());
            args.push(v.to_vec());
        }
        match self.codec_mut().request(&args).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HGET key field`. `None` if key or field absent.
    pub async fn hget(&mut self, key: &[u8], field: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.codec_mut().request(&vec3(b"HGET", key, field)).await? {
            Reply::Bulk(v) => Ok(Some(v)),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HDEL key field [field ...]`. Returns count actually removed.
    pub async fn hdel(&mut self, key: &[u8], fields: &[&[u8]]) -> io::Result<usize> {
        let mut args = Vec::with_capacity(fields.len() + 2);
        args.push(b"HDEL".to_vec());
        args.push(key.to_vec());
        args.extend(fields.iter().map(|f| f.to_vec()));
        match self.codec_mut().request(&args).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HLEN key`. 0 if absent.
    pub async fn hlen(&mut self, key: &[u8]) -> io::Result<usize> {
        match self.codec_mut().request(&vec2(b"HLEN", key)).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HGETALL key`. Flat `[f0, v0, f1, v1, ...]`. Empty if absent.
    pub async fn hgetall(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self.codec_mut().request(&vec2(b"HGETALL", key)).await? {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HKEYS key`. Hash's field names.
    pub async fn hkeys(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self.codec_mut().request(&vec2(b"HKEYS", key)).await? {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HVALS key`. Hash's values.
    pub async fn hvals(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self.codec_mut().request(&vec2(b"HVALS", key)).await? {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}
