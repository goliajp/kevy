//! Async mirror of list commands on `kevy_client::Connection`.
//!
//! ```
//! # include!("doc_serve.rs");
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! # let addr = serve(&[("RPUSH jobs a b", ":2\r\n"), ("LPOP jobs 1", "*1\r\n$1\r\na\r\n")]).await?;
//! use kevy_client_async::AsyncConnection;
//!
//! let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
//! c.rpush(b"jobs", &[b"a", b"b"]).await?;
//! assert_eq!(c.lpop(b"jobs", 1).await?, [b"a"]); // first in, first out
//! # Ok(()) }
//! ```

use std::io;

use kevy_resp::Reply;

use crate::codec::AsyncRespCodec;
use crate::conn::AsyncConnection;
use crate::reply::{array_to_bulks, string, unexpected};
use crate::transport::AsyncTransport;

impl<T: crate::AsyncTransport> AsyncConnection<T> {
    /// `LPUSH key value [value ...]`. Returns new list length.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("LPUSH recent x y", ":2\r\n"), ("LRANGE recent 0 -1", "*2\r\n$1\r\ny\r\n$1\r\nx\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.lpush(b"recent", &[b"x", b"y"]).await?, 2);
    /// // each value went to the head in turn
    /// assert_eq!(c.lrange(b"recent", 0, -1).await?, [b"y", b"x"]);
    /// # Ok(()) }
    /// ```
    pub async fn lpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        list_push(self.codec_mut(), b"LPUSH", key, values).await
    }

    /// `RPUSH key value [value ...]`. Returns new list length.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("RPUSH jobs a b", ":2\r\n"), ("RPUSH jobs c", ":3\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.rpush(b"jobs", &[b"a", b"b"]).await?, 2);
    /// assert_eq!(c.rpush(b"jobs", &[b"c"]).await?, 3);
    /// # Ok(()) }
    /// ```
    pub async fn rpush(&mut self, key: &[u8], values: &[&[u8]]) -> io::Result<usize> {
        list_push(self.codec_mut(), b"RPUSH", key, values).await
    }

    /// `LPOP key count`. Returns up to `count` head values; empty if
    /// absent / drained.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("LPOP jobs 2", "*2\r\n$1\r\na\r\n$1\r\nb\r\n"), ("LPOP missing 2", "*-1\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.lpop(b"jobs", 2).await?, [b"a", b"b"]);
    /// assert!(c.lpop(b"missing", 2).await?.is_empty());
    /// # Ok(()) }
    /// ```
    pub async fn lpop(&mut self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        list_pop(self.codec_mut(), b"LPOP", key, count).await
    }

    /// `RPOP key count`. Symmetric to `lpop` from the tail.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("RPOP jobs 1", "*1\r\n$1\r\nc\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.rpop(b"jobs", 1).await?, [b"c"]);
    /// # Ok(()) }
    /// ```
    pub async fn rpop(&mut self, key: &[u8], count: usize) -> io::Result<Vec<Vec<u8>>> {
        list_pop(self.codec_mut(), b"RPOP", key, count).await
    }

    /// `LLEN key`. 0 if absent.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("LLEN jobs", ":3\r\n"), ("LLEN missing", ":0\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.llen(b"jobs").await?, 3);
    /// assert_eq!(c.llen(b"missing").await?, 0);
    /// # Ok(()) }
    /// ```
    pub async fn llen(&mut self, key: &[u8]) -> io::Result<usize> {
        match self.codec_mut().request_borrowed(&[b"LLEN", key]).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `LRANGE key start stop`. Negative offsets count from tail.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("LRANGE jobs -2 -1", "*2\r\n$1\r\nb\r\n$1\r\nc\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// // the last two elements
    /// assert_eq!(c.lrange(b"jobs", -2, -1).await?, [b"b", b"c"]);
    /// # Ok(()) }
    /// ```
    pub async fn lrange(&mut self, key: &[u8], start: i64, stop: i64) -> io::Result<Vec<Vec<u8>>> {
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
