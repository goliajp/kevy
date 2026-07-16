//! Async mirror of list commands on `kevy_client::Connection`.

use std::io;

use kevy_resp::Reply;

use crate::codec::AsyncRespCodec;
use crate::conn::AsyncConnection;
use crate::reply::{array_to_bulks, string, unexpected};
use crate::transport::AsyncTransport;

impl AsyncConnection {
    /// `LPUSH key value [value ...]`. Returns new list length.
    pub async fn lpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        list_push(self.codec_mut(), b"LPUSH", key, values).await
    }

    /// `RPUSH key value [value ...]`. Returns new list length.
    pub async fn rpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        list_push(self.codec_mut(), b"RPUSH", key, values).await
    }

    /// `LPOP key count`. Returns up to `count` head values; empty if
    /// absent / drained.
    pub async fn lpop(&mut self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        list_pop(self.codec_mut(), b"LPOP", key, count).await
    }

    /// `RPOP key count`. Symmetric to `lpop` from the tail.
    pub async fn rpop(&mut self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        list_pop(self.codec_mut(), b"RPOP", key, count).await
    }

    /// `LLEN key`. 0 if absent.
    pub async fn llen(&mut self, key: &[u8]) -> io::Result<usize> {
        match self.codec_mut().request_borrowed(&[b"LLEN", key]).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `LRANGE key start stop`. Negative offsets count from tail.
    pub async fn lrange(
        &mut self,
        key: &[u8],
        start: i64,
        stop: i64,
    ) -> io::Result<Vec<Vec<u8>>> {
        let start_s = start.to_string();
        let stop_s = stop.to_string();
        match self
            .codec_mut()
            .request_borrowed(&[b"LRANGE", key, start_s.as_bytes(), stop_s.as_bytes()])
            .await?
        {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}

async fn list_push<T: AsyncTransport>(
    c: &mut AsyncRespCodec<T>,
    verb: &[u8],
    key: &[u8],
    values: &[&[u8]],
) -> io::Result<usize> {
    let mut args: Vec<&[u8]> = Vec::with_capacity(values.len() + 2);
    args.push(verb);
    args.push(key);
    args.extend_from_slice(values);
    match c.request_borrowed(&args).await? {
        Reply::Int(n) if n >= 0 => Ok(n as usize),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}

async fn list_pop<T: AsyncTransport>(
    c: &mut AsyncRespCodec<T>,
    verb: &[u8],
    key: &[u8],
    count: usize,
) -> io::Result<Vec<Vec<u8>>> {
    let count_s = count.to_string();
    match c.request_borrowed(&[verb, key, count_s.as_bytes()]).await? {
        Reply::Array(items) => array_to_bulks(items),
        Reply::Bulk(v) => Ok(vec![v]),
        Reply::Nil => Ok(Vec::new()),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}
