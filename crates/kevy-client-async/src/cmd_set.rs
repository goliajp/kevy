//! Async mirror of set commands on `kevy_client::Connection`.

use std::io;

use kevy_resp::Reply;

use crate::codec::AsyncRespCodec;
use crate::conn::AsyncConnection;
use crate::reply::{array_to_bulks, string, unexpected, vec2, vec3};
use crate::transport::AsyncTransport;

impl AsyncConnection {
    /// `SADD key member [member ...]`. Returns count of newly added.
    pub async fn sadd(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        set_multi(self.codec_mut(), b"SADD", key, members).await
    }

    /// `SREM key member [member ...]`. Returns count actually removed.
    pub async fn srem(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        set_multi(self.codec_mut(), b"SREM", key, members).await
    }

    /// `SMEMBERS key`. Implementation-defined order; empty if absent.
    pub async fn smembers(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self.codec_mut().request(&vec2(b"SMEMBERS", key)).await? {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SCARD key`. 0 if absent.
    pub async fn scard(&mut self, key: &[u8]) -> io::Result<usize> {
        match self.codec_mut().request(&vec2(b"SCARD", key)).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SISMEMBER key member`. `false` if absent.
    pub async fn sismember(&mut self, key: &[u8], member: &[u8]) -> io::Result<bool> {
        match self
            .codec_mut()
            .request(&vec3(b"SISMEMBER", key, member))
            .await?
        {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SINTER key [key ...]` — intersection of all sets.
    pub async fn sinter(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        set_combine(self.codec_mut(), b"SINTER", keys).await
    }

    /// `SUNION key [key ...]` — union of all sets.
    pub async fn sunion(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        set_combine(self.codec_mut(), b"SUNION", keys).await
    }

    /// `SDIFF key [key ...]` — first set minus the rest.
    pub async fn sdiff(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        set_combine(self.codec_mut(), b"SDIFF", keys).await
    }
}

pub(crate) async fn set_multi<T: AsyncTransport>(
    c: &mut AsyncRespCodec<T>,
    verb: &[u8],
    key: &[u8],
    members: &[&[u8]],
) -> io::Result<usize> {
    let mut args = Vec::with_capacity(members.len() + 2);
    args.push(verb.to_vec());
    args.push(key.to_vec());
    args.extend(members.iter().map(|m| m.to_vec()));
    match c.request(&args).await? {
        Reply::Int(n) if n >= 0 => Ok(n as usize),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

async fn set_combine<T: AsyncTransport>(
    c: &mut AsyncRespCodec<T>,
    verb: &[u8],
    keys: &[&[u8]],
) -> io::Result<Vec<Vec<u8>>> {
    let mut args = Vec::with_capacity(keys.len() + 1);
    args.push(verb.to_vec());
    args.extend(keys.iter().map(|k| k.to_vec()));
    match c.request(&args).await? {
        Reply::Array(items) => array_to_bulks(items),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}
