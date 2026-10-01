//! Async mirror of the string + generic key commands on
//! [`kevy_client::Connection`](https://docs.rs/kevy-client/latest/kevy_client/enum.Connection.html). Each method here is a 1:1 translation
//! of the corresponding blocking method: same name, same arguments,
//! same return type modulo `.await`.
//!
//! ```
//! # include!("doc_serve.rs");
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! # let addr = serve(&[("SET greeting hello", "+OK\r\n"), ("GET greeting", "$5\r\nhello\r\n"), ("DEL greeting", ":1\r\n")]).await?;
//! use kevy_client_async::AsyncConnection;
//!
//! let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
//! c.set(b"greeting", b"hello").await?;
//! assert_eq!(c.get(b"greeting").await?.as_deref(), Some(&b"hello"[..]));
//! assert_eq!(c.del(&[b"greeting"]).await?, 1);
//! # Ok(()) }
//! ```

use std::io;
use std::time::Duration;

use kevy_resp::Reply;

use crate::conn::AsyncConnection;
use crate::reply::{string, unexpected};

impl<T: crate::AsyncTransport> AsyncConnection<T> {
    /// `SET key value`. Unconditional set; returns on `+OK`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SET greeting hello", "+OK\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// c.set(b"greeting", b"hello").await?;
    /// # Ok(()) }
    /// ```
    pub async fn set(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        match self.codec_mut().request_borrowed(&[b"SET", key, value]).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `GET key`. `None` if absent or expired.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("GET greeting", "$5\r\nhello\r\n"), ("GET missing", "$-1\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.get(b"greeting").await?.as_deref(), Some(&b"hello"[..]));
    /// assert_eq!(c.get(b"missing").await?, None);
    /// # Ok(()) }
    /// ```
    pub async fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.codec_mut().request_borrowed(&[b"GET", key]).await? {
            Reply::Bulk(v) => Ok(Some(v)),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `DEL key [key ...]`. Returns the count actually removed.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("DEL a b", ":1\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// // only `a` existed
    /// assert_eq!(c.del(&[b"a", b"b"]).await?, 1);
    /// # Ok(()) }
    /// ```
    pub async fn del(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
        args.push(b"DEL");
        args.extend_from_slice(keys);
        match self.codec_mut().request_borrowed(&args).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `EXISTS key [key ...]`. Count of keys present (a key passed N
    /// times counts N if it exists).
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("EXISTS a a b", ":2\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// // `a` exists and counts once per mention; `b` does not exist
    /// assert_eq!(c.exists(&[b"a", b"a", b"b"]).await?, 2);
    /// # Ok(()) }
    /// ```
    pub async fn exists(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
        args.push(b"EXISTS");
        args.extend_from_slice(keys);
        match self.codec_mut().request_borrowed(&args).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCR key`. Returns post-increment value.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("INCR hits", ":1\r\n"), ("INCR hits", ":2\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.incr(b"hits").await?, 1);
    /// assert_eq!(c.incr(b"hits").await?, 2);
    /// # Ok(()) }
    /// ```
    pub async fn incr(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.codec_mut().request_borrowed(&[b"INCR", key]).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCRBY key delta`. Negative delta = `DECRBY`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("INCRBY stock 10", ":10\r\n"), ("INCRBY stock -3", ":7\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.incr_by(b"stock", 10).await?, 10);
    /// assert_eq!(c.incr_by(b"stock", -3).await?, 7);
    /// # Ok(()) }
    /// ```
    pub async fn incr_by(&mut self, key: &[u8], delta: i64) -> io::Result<i64> {
        let delta_s = delta.to_string();
        match self.codec_mut().request_borrowed(&[b"INCRBY", key, delta_s.as_bytes()]).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PEXPIRE key ttl_ms`. Returns whether the key existed and got
    /// a TTL set.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("PEXPIRE session 1500", ":1\r\n"), ("PEXPIRE missing 1500", ":0\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// use std::time::Duration;
    /// assert!(c.expire(b"session", Duration::from_millis(1500)).await?);
    /// assert!(!c.expire(b"missing", Duration::from_millis(1500)).await?);
    /// # Ok(()) }
    /// ```
    pub async fn expire(&mut self, key: &[u8], ttl: Duration) -> io::Result<bool> {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        let ms_s = ms.to_string();
        match self.codec_mut().request_borrowed(&[b"PEXPIRE", key, ms_s.as_bytes()]).await? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PERSIST key`. Returns whether a TTL was removed.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("PERSIST session", ":1\r\n"), ("PERSIST session", ":0\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert!(c.persist(b"session").await?); // the TTL is gone
    /// assert!(!c.persist(b"session").await?); // nothing left to remove
    /// # Ok(()) }
    /// ```
    pub async fn persist(&mut self, key: &[u8]) -> io::Result<bool> {
        match self.codec_mut().request_borrowed(&[b"PERSIST", key]).await? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PTTL key`. Ms remaining, -2 if no key, -1 if no TTL.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("PTTL session", ":1200\r\n"), ("PTTL forever", ":-1\r\n"), ("PTTL missing", ":-2\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.ttl_ms(b"session").await?, 1200);
    /// assert_eq!(c.ttl_ms(b"forever").await?, -1); // no TTL
    /// assert_eq!(c.ttl_ms(b"missing").await?, -2); // no key
    /// # Ok(()) }
    /// ```
    pub async fn ttl_ms(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.codec_mut().request_borrowed(&[b"PTTL", key]).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `TYPE key`. Returns Redis-style type name (`"string"`, `"hash"`,
    /// `"list"`, `"set"`, `"zset"`, or `"none"`).
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("TYPE user:1", "+hash\r\n"), ("TYPE missing", "+none\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.type_of(b"user:1").await?, "hash");
    /// assert_eq!(c.type_of(b"missing").await?, "none");
    /// # Ok(()) }
    /// ```
    pub async fn type_of(&mut self, key: &[u8]) -> io::Result<String> {
        match self.codec_mut().request_borrowed(&[b"TYPE", key]).await? {
            Reply::Simple(s) => Ok(string(s)),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `DBSIZE`. Total live keys at call time.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("DBSIZE", ":42\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// assert_eq!(c.dbsize().await?, 42);
    /// # Ok(()) }
    /// ```
    pub async fn dbsize(&mut self) -> io::Result<usize> {
        match self.codec_mut().request_borrowed(&[b"DBSIZE"]).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `FLUSHALL`. WIPES the store. Named `flushall` not `flush` to
    /// avoid colliding with `Write::flush`'s sync-to-disk meaning.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("FLUSHALL", "+OK\r\n"), ("DBSIZE", ":0\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// c.flushall().await?;
    /// assert_eq!(c.dbsize().await?, 0);
    /// # Ok(()) }
    /// ```
    pub async fn flushall(&mut self) -> io::Result<()> {
        match self.codec_mut().request_borrowed(&[b"FLUSHALL"]).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SET key value PX ttl_ms`. Atomic cache-with-expiry.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SET token abc PX 30000", "+OK\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// use std::time::Duration;
    /// c.set_with_ttl(b"token", b"abc", Duration::from_secs(30)).await?;
    /// # Ok(()) }
    /// ```
    pub async fn set_with_ttl(
        &mut self,
        key: &[u8],
        value: &[u8],
        ttl: Duration,
    ) -> io::Result<()> {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        let ms_s = ms.to_string();
        match self
            .codec_mut()
            .request_borrowed(&[b"SET", key, value, b"PX", ms_s.as_bytes()])
            .await?
        {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `MGET key [key ...]` — one reply per key, in order.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("MGET a missing", "*2\r\n$1\r\n1\r\n$-1\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// let values = c.mget(&[b"a", b"missing"]).await?;
    /// assert_eq!(values, [Some(b"1".to_vec()), None]);
    /// # Ok(()) }
    /// ```
    pub async fn mget(&mut self, keys: &[&[u8]]) -> io::Result<Vec<Option<Vec<u8>>>> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
        args.push(b"MGET");
        args.extend_from_slice(keys);
        match self.codec_mut().request_borrowed(&args).await? {
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
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("MSET a 1 b 2", "+OK\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// c.mset(&[(b"a", b"1"), (b"b", b"2")]).await?;
    /// # Ok(()) }
    /// ```
    pub async fn mset(&mut self, pairs: &[(&[u8], &[u8])]) -> io::Result<()> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(pairs.len() * 2 + 1);
        args.push(b"MSET");
        for &(k, v) in pairs {
            args.push(k);
            args.push(v);
        }
        match self.codec_mut().request_borrowed(&args).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PUBLISH channel message`. Returns subscriber-receive count.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("PUBLISH news hello", ":2\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// // two subscribers received it
    /// assert_eq!(c.publish(b"news", b"hello").await?, 2);
    /// # Ok(()) }
    /// ```
    pub async fn publish(&mut self, channel: &[u8], message: &[u8]) -> io::Result<usize> {
        match self.codec_mut().request_borrowed(&[b"PUBLISH", channel, message]).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}
