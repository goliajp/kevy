//! Async mirror of set commands on `kevy_client::Connection`.
//!
//! ```
//! # include!("doc_serve.rs");
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! # let addr = serve(&[("SADD tags rust db", ":2\r\n"), ("SISMEMBER tags rust", ":1\r\n")]).await?;
//! use kevy_client_async::AsyncConnection;
//!
//! let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
//! c.sadd(b"tags", &[b"rust", b"db"]).await?;
//! assert!(c.sismember(b"tags", b"rust").await?);
//! # Ok(()) }
//! ```

use std::io;

use kevy_resp::Reply;

use crate::codec::AsyncRespCodec;
use crate::conn::AsyncConnection;
use crate::reply::{array_to_bulks, string, unexpected};
use crate::transport::AsyncTransport;

impl<T: crate::AsyncTransport> AsyncConnection<T> {
    /// `SADD key member [member ...]`. Returns count of newly added.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SADD tags rust db", ":2\r\n"), ("SADD tags rust", ":0\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.sadd(b"tags", &[b"rust", b"db"]).await?, 2);
    /// assert_eq!(c.sadd(b"tags", &[b"rust"]).await?, 0); // already a member
    /// # Ok(()) }
    /// ```
    pub async fn sadd(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        set_multi(self.codec_mut(), b"SADD", key, members).await
    }

    /// `SREM key member [member ...]`. Returns count actually removed.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SREM tags db go", ":1\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.srem(b"tags", &[b"db", b"go"]).await?, 1); // `go` was never in
    /// # Ok(()) }
    /// ```
    pub async fn srem(&mut self, key: &[u8], members: &[&[u8]]) -> io::Result<usize> {
        set_multi(self.codec_mut(), b"SREM", key, members).await
    }

    /// `SMEMBERS key`. Implementation-defined order; empty if absent.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SMEMBERS tags", "*2\r\n$2\r\ndb\r\n$4\r\nrust\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// let mut members = c.smembers(b"tags").await?;
    /// members.sort(); // the order is up to the server
    /// assert_eq!(members, [&b"db"[..], b"rust"]);
    /// # Ok(()) }
    /// ```
    pub async fn smembers(&mut self, key: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        match self.codec_mut().request_borrowed(&[b"SMEMBERS", key]).await? {
            Reply::Array(items) => array_to_bulks(items),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SCARD key`. 0 if absent.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SCARD tags", ":2\r\n"), ("SCARD missing", ":0\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.scard(b"tags").await?, 2);
    /// assert_eq!(c.scard(b"missing").await?, 0);
    /// # Ok(()) }
    /// ```
    pub async fn scard(&mut self, key: &[u8]) -> io::Result<usize> {
        match self.codec_mut().request_borrowed(&[b"SCARD", key]).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SISMEMBER key member`. `false` if absent.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SISMEMBER tags rust", ":1\r\n"), ("SISMEMBER tags go", ":0\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert!(c.sismember(b"tags", b"rust").await?);
    /// assert!(!c.sismember(b"tags", b"go").await?);
    /// # Ok(()) }
    /// ```
    pub async fn sismember(&mut self, key: &[u8], member: &[u8]) -> io::Result<bool> {
        match self.codec_mut().request_borrowed(&[b"SISMEMBER", key, member]).await? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SINTER key [key ...]` — intersection of all sets.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SINTER a b", "*1\r\n$1\r\nx\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// // a = {x, y}, b = {x, z}
    /// assert_eq!(c.sinter(&[b"a", b"b"]).await?, [b"x"]);
    /// # Ok(()) }
    /// ```
    pub async fn sinter(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        set_combine(self.codec_mut(), b"SINTER", keys).await
    }

    /// `SUNION key [key ...]` — union of all sets.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SUNION a b", "*3\r\n$1\r\nx\r\n$1\r\ny\r\n$1\r\nz\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// // a = {x, y}, b = {x, z}
    /// let mut all = c.sunion(&[b"a", b"b"]).await?;
    /// all.sort();
    /// assert_eq!(all, [b"x", b"y", b"z"]);
    /// # Ok(()) }
    /// ```
    pub async fn sunion(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Vec<u8>>> {
        set_combine(self.codec_mut(), b"SUNION", keys).await
    }

    /// `SDIFF key [key ...]` — first set minus the rest.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SDIFF a b", "*1\r\n$1\r\ny\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// // a = {x, y}, b = {x, z}
    /// assert_eq!(c.sdiff(&[b"a", b"b"]).await?, [b"y"]);
    /// # Ok(()) }
    /// ```
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
    let mut args: Vec<&[u8]> = Vec::with_capacity(members.len() + 2);
    args.push(verb);
    args.push(key);
    args.extend_from_slice(members);
    match c.request_borrowed(&args).await? {
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
    let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
    args.push(verb);
    args.extend_from_slice(keys);
    match c.request_borrowed(&args).await? {
        Reply::Array(items) => array_to_bulks(items),
        Reply::Error(e) => Err(io::Error::other(string(e))),
        other => Err(unexpected(other)),
    }
}
