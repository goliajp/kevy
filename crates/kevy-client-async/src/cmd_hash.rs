//! Async mirror of hash commands on `kevy_client::Connection`.
//!
//! ```
//! # include!("doc_serve.rs");
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! # let addr = serve(&[("HSET user:1 name ada lang rust", ":2\r\n"), ("HGET user:1 name", "$3\r\nada\r\n")]).await?;
//! use kevy_client_async::AsyncConnection;
//!
//! let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
//! c.hset(b"user:1", &[(b"name", b"ada"), (b"lang", b"rust")]).await?;
//! assert_eq!(c.hget(b"user:1", b"name").await?.as_deref(), Some(&b"ada"[..]));
//! # Ok(()) }
//! ```

use std::io;

use kevy_resp::Reply;

use crate::conn::AsyncConnection;
use crate::reply::{array_to_bulks, string, unexpected};

impl<T: crate::AsyncTransport> AsyncConnection<T> {
    /// `HSET key field value [field value ...]`. Returns count of
    /// fields newly created (overwrites don't count).
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("HSET user:1 name ada lang rust", ":2\r\n"), ("HSET user:1 name grace", ":0\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.hset(b"user:1", &[(b"name", b"ada"), (b"lang", b"rust")]).await?, 2);
    /// // an overwrite creates no field
    /// assert_eq!(c.hset(b"user:1", &[(b"name", b"grace")]).await?, 0);
    /// # Ok(()) }
    /// ```
    pub async fn hset(&mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> io::Result<usize> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(2 + pairs.len() * 2);
        args.push(b"HSET");
        args.push(key);
        for &(f, v) in pairs {
            args.push(f);
            args.push(v);
        }
        match self.codec_mut().request_borrowed(&args).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HGET key field`. `None` if key or field absent.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("HGET user:1 name", "$3\r\nada\r\n"), ("HGET user:1 age", "$-1\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.hget(b"user:1", b"name").await?.as_deref(), Some(&b"ada"[..]));
    /// assert_eq!(c.hget(b"user:1", b"age").await?, None);
    /// # Ok(()) }
    /// ```
    pub async fn hget(&mut self, key: &[u8], field: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.codec_mut().request_borrowed(&[b"HGET", key, field]).await? {
            Reply::Bulk(v) => Ok(Some(v)),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HDEL key field [field ...]`. Returns count actually removed.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("HDEL user:1 lang age", ":1\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// // only `lang` was there
    /// assert_eq!(c.hdel(b"user:1", &[b"lang", b"age"]).await?, 1);
    /// # Ok(()) }
    /// ```
    pub async fn hdel(&mut self, key: &[u8], fields: &[&[u8]]) -> io::Result<usize> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(fields.len() + 2);
        args.push(b"HDEL");
        args.push(key);
        args.extend_from_slice(fields);
        match self.codec_mut().request_borrowed(&args).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HLEN key`. 0 if absent.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("HLEN user:1", ":2\r\n"), ("HLEN missing", ":0\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.hlen(b"user:1").await?, 2);
    /// assert_eq!(c.hlen(b"missing").await?, 0);
    /// # Ok(()) }
    /// ```
    pub async fn hlen(&mut self, key: &[u8]) -> io::Result<usize> {
        match self.codec_mut().request_borrowed(&[b"HLEN", key]).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HGETALL key`. Flat `[f0, v0, f1, v1, ...]`. Empty if absent.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("HGETALL user:1", "*4\r\n$4\r\nname\r\n$3\r\nada\r\n$4\r\nlang\r\n$4\r\nrust\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// let flat = c.hgetall(b"user:1").await?;
    /// assert_eq!(flat, [&b"name"[..], b"ada", b"lang", b"rust"]);
    /// # Ok(()) }
    /// ```
    pub async fn hgetall(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self.codec_mut().request_borrowed(&[b"HGETALL", key]).await? {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HKEYS key`. Hash's field names.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("HKEYS user:1", "*2\r\n$4\r\nname\r\n$4\r\nlang\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.hkeys(b"user:1").await?, [&b"name"[..], b"lang"]);
    /// # Ok(()) }
    /// ```
    pub async fn hkeys(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self.codec_mut().request_borrowed(&[b"HKEYS", key]).await? {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `HVALS key`. Hash's values.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("HVALS user:1", "*2\r\n$3\r\nada\r\n$4\r\nrust\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.hvals(b"user:1").await?, [&b"ada"[..], b"rust"]);
    /// # Ok(()) }
    /// ```
    pub async fn hvals(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self.codec_mut().request_borrowed(&[b"HVALS", key]).await? {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}
