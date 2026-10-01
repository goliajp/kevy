//! Pipeline-first sugar — where async actually pays off: one TCP
//! round-trip per batch instead of per command.
//!
//! ```
//! # include!("doc_serve.rs");
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! # let addr = serve(&[("SET k1 v1", "+OK\r\n"), ("GET k2", "$-1\r\n"), ("INCR counter", ":1\r\n")]).await?;
//! # let mut conn = kevy_client_async::AsyncConnection::connect(&format!("kevy://{addr}")).await?;
//! let replies = conn.pipeline()
//!     .set(b"k1", b"v1")
//!     .get(b"k2")
//!     .incr(b"counter")
//!     .run(&mut conn).await?;
//! assert_eq!(replies.len(), 3);
//! # Ok(()) }
//! ```
//!
//! The builder owns no connection — it just accumulates argv vectors.
//! `run(&mut conn)` writes all commands as one buffered `write_all`
//! and then drains N replies in declaration order via the same codec
//! the rest of the crate uses.
//!
//! # Partial-failure semantics
//!
//! `run()` returns `Result<Vec<Reply>, io::Error>`. The outer `Err` is
//! reserved for connection-level failures (transport error, malformed
//! frame). **Per-command** errors surface inside the `Vec<Reply>` as
//! `Reply::Error(_)` entries — the rest of the batch is unaffected.
//! Callers iterate and decide which to ignore vs propagate:
//!
//! ```
//! # include!("doc_serve.rs");
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! # let addr = serve(&[("SET k v", "+OK\r\n"), ("INCR k", "-ERR value is not an integer\r\n")]).await?;
//! # let mut conn = kevy_client_async::AsyncConnection::connect(&format!("kevy://{addr}")).await?;
//! use kevy_resp::Reply;
//!
//! let replies = conn.pipeline().set(b"k", b"v").incr(b"k").run(&mut conn).await?;
//! let mut failed = Vec::new();
//! for (i, r) in replies.iter().enumerate() {
//!     if let Reply::Error(msg) = r {
//!         failed.push((i, String::from_utf8_lossy(msg).into_owned()));
//!     }
//! }
//! // the SET went through; only the INCR failed
//! assert_eq!(failed, [(1, "ERR value is not an integer".to_string())]);
//! # Ok(()) }
//! ```
//!
//! # Degrade path
//!
//! [`Pipeline::into_cmds`] hands back the raw argv vectors so callers
//! can feed them into a blocking client one at a time. Same builder,
//! same accumulated state — only the executor changes.
//!
//! ```
//! use kevy_client_async::pipeline::Pipeline;
//!
//! let cmds = Pipeline::new().get(b"a").set(b"b", b"v").into_cmds();
//! assert_eq!(cmds, [vec![&b"GET"[..], b"a"], vec![&b"SET"[..], b"b", b"v"]]);
//! // on a blocking kevy_client::Connection:
//! // for cmd in &cmds { blocking_resp.request(cmd)?; }
//! ```

use std::io;
use std::time::Duration;

use kevy_resp::Reply;

use crate::conn::AsyncConnection;
use crate::reply::{vec2, vec3};

/// Accumulating command builder. Created via
/// [`AsyncConnection::pipeline`]. Owns no connection — that's bound
/// at [`Pipeline::run`] time.
///
/// ```
/// use kevy_client_async::pipeline::Pipeline;
///
///
/// let p = Pipeline::new().set(b"k", b"v").get(b"k");
/// assert_eq!(p.len(), 2);
/// // the builder is a value: clone it to run the same batch twice
/// let again = p.clone();
/// assert_eq!(p.into_cmds(), again.into_cmds());
/// ```
#[derive(Debug, Default, Clone)]
pub struct Pipeline {
    cmds: Vec<Vec<Vec<u8>>>,
}

impl Pipeline {
    /// Empty pipeline.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    ///
    /// let p = Pipeline::new();
    /// assert!(p.is_empty());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of queued commands.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    ///
    /// assert_eq!(Pipeline::new().incr(b"a").incr(b"b").len(), 2);
    /// ```
    pub fn len(&self) -> usize {
        self.cmds.len()
    }

    /// True if no commands are queued yet.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    ///
    /// assert!(Pipeline::new().is_empty());
    /// assert!(!Pipeline::new().get(b"k").is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }

    /// Append a pre-built RESP argv. Escape hatch for commands the
    /// typed builder methods don't cover.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().push_raw(vec![b"OBJECT".to_vec(), b"ENCODING".to_vec(), b"k".to_vec()]).into_cmds();
    /// assert_eq!(cmds, [[&b"OBJECT"[..], b"ENCODING", b"k"]]);
    /// ```
    pub fn push_raw(mut self, argv: Vec<Vec<u8>>) -> Self {
        self.cmds.push(argv);
        self
    }

    // ── String + generic key commands ─────────────────────────────

    /// Queue `GET key`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().get(b"k").into_cmds();
    /// assert_eq!(cmds, [[&b"GET"[..], b"k"]]);
    /// ```
    pub fn get(mut self, key: &[u8]) -> Self {
        self.cmds.push(vec2(b"GET", key));
        self
    }

    /// Queue `SET key value`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().set(b"k", b"v").into_cmds();
    /// assert_eq!(cmds, [[&b"SET"[..], b"k", b"v"]]);
    /// ```
    pub fn set(mut self, key: &[u8], value: &[u8]) -> Self {
        self.cmds.push(vec3(b"SET", key, value));
        self
    }

    /// Queue `SET key value PX ttl_ms`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    ///
    /// use std::time::Duration;
    ///
    /// let cmds = Pipeline::new().set_with_ttl(b"k", b"v", Duration::from_secs(2)).into_cmds();
    /// assert_eq!(cmds, [[&b"SET"[..], b"k", b"v", b"PX", b"2000"]]);
    /// ```
    pub fn set_with_ttl(mut self, key: &[u8], value: &[u8], ttl: Duration) -> Self {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        self.cmds.push(vec![
            b"SET".to_vec(),
            key.to_vec(),
            value.to_vec(),
            b"PX".to_vec(),
            ms.to_string().into_bytes(),
        ]);
        self
    }

    /// Queue `DEL key [key ...]`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().del(&[b"a", b"b"]).into_cmds();
    /// assert_eq!(cmds, [[&b"DEL"[..], b"a", b"b"]]);
    /// ```
    pub fn del(mut self, keys: &[&[u8]]) -> Self {
        let mut argv = Vec::with_capacity(keys.len() + 1);
        argv.push(b"DEL".to_vec());
        argv.extend(keys.iter().map(|k| k.to_vec()));
        self.cmds.push(argv);
        self
    }

    /// Queue `EXISTS key [key ...]`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().exists(&[b"a", b"b"]).into_cmds();
    /// assert_eq!(cmds, [[&b"EXISTS"[..], b"a", b"b"]]);
    /// ```
    pub fn exists(mut self, keys: &[&[u8]]) -> Self {
        let mut argv = Vec::with_capacity(keys.len() + 1);
        argv.push(b"EXISTS".to_vec());
        argv.extend(keys.iter().map(|k| k.to_vec()));
        self.cmds.push(argv);
        self
    }

    /// Queue `INCR key`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().incr(b"hits").into_cmds();
    /// assert_eq!(cmds, [[&b"INCR"[..], b"hits"]]);
    /// ```
    pub fn incr(mut self, key: &[u8]) -> Self {
        self.cmds.push(vec2(b"INCR", key));
        self
    }

    /// Queue `INCRBY key delta`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().incr_by(b"stock", -3).into_cmds();
    /// assert_eq!(cmds, [[&b"INCRBY"[..], b"stock", b"-3"]]);
    /// ```
    pub fn incr_by(mut self, key: &[u8], delta: i64) -> Self {
        self.cmds.push(vec![b"INCRBY".to_vec(), key.to_vec(), delta.to_string().into_bytes()]);
        self
    }

    /// Queue `PEXPIRE key ttl_ms`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    ///
    /// use std::time::Duration;
    ///
    /// let cmds = Pipeline::new().expire(b"k", Duration::from_millis(1500)).into_cmds();
    /// assert_eq!(cmds, [[&b"PEXPIRE"[..], b"k", b"1500"]]);
    /// ```
    pub fn expire(mut self, key: &[u8], ttl: Duration) -> Self {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        self.cmds.push(vec![b"PEXPIRE".to_vec(), key.to_vec(), ms.to_string().into_bytes()]);
        self
    }

    /// Queue `PUBLISH channel message`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().publish(b"news", b"hi").into_cmds();
    /// assert_eq!(cmds, [[&b"PUBLISH"[..], b"news", b"hi"]]);
    /// ```
    pub fn publish(mut self, channel: &[u8], message: &[u8]) -> Self {
        self.cmds.push(vec3(b"PUBLISH", channel, message));
        self
    }

    /// Queue `HGET key field`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().hget(b"user:1", b"name").into_cmds();
    /// assert_eq!(cmds, [[&b"HGET"[..], b"user:1", b"name"]]);
    /// ```
    pub fn hget(mut self, key: &[u8], field: &[u8]) -> Self {
        self.cmds.push(vec3(b"HGET", key, field));
        self
    }

    /// Queue `HSET key field value [field value ...]`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().hset(b"user:1", &[(b"name", b"ada")]).into_cmds();
    /// assert_eq!(cmds, [[&b"HSET"[..], b"user:1", b"name", b"ada"]]);
    /// ```
    pub fn hset(mut self, key: &[u8], pairs: &[(&[u8], &[u8])]) -> Self {
        let mut argv = Vec::with_capacity(2 + pairs.len() * 2);
        argv.push(b"HSET".to_vec());
        argv.push(key.to_vec());
        for (f, v) in pairs {
            argv.push(f.to_vec());
            argv.push(v.to_vec());
        }
        self.cmds.push(argv);
        self
    }

    /// Queue `LPUSH key value [value ...]`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().lpush(b"l", &[b"x", b"y"]).into_cmds();
    /// assert_eq!(cmds, [[&b"LPUSH"[..], b"l", b"x", b"y"]]);
    /// ```
    pub fn lpush(mut self, key: &[u8], values: &[&[u8]]) -> Self {
        let mut argv = Vec::with_capacity(values.len() + 2);
        argv.push(b"LPUSH".to_vec());
        argv.push(key.to_vec());
        argv.extend(values.iter().map(|v| v.to_vec()));
        self.cmds.push(argv);
        self
    }

    /// Queue `RPUSH key value [value ...]`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().rpush(b"l", &[b"x", b"y"]).into_cmds();
    /// assert_eq!(cmds, [[&b"RPUSH"[..], b"l", b"x", b"y"]]);
    /// ```
    pub fn rpush(mut self, key: &[u8], values: &[&[u8]]) -> Self {
        let mut argv = Vec::with_capacity(values.len() + 2);
        argv.push(b"RPUSH".to_vec());
        argv.push(key.to_vec());
        argv.extend(values.iter().map(|v| v.to_vec()));
        self.cmds.push(argv);
        self
    }

    /// Queue `SADD key member [member ...]`.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().sadd(b"s", &[b"x"]).into_cmds();
    /// assert_eq!(cmds, [[&b"SADD"[..], b"s", b"x"]]);
    /// ```
    pub fn sadd(mut self, key: &[u8], members: &[&[u8]]) -> Self {
        let mut argv = Vec::with_capacity(members.len() + 2);
        argv.push(b"SADD".to_vec());
        argv.push(key.to_vec());
        argv.extend(members.iter().map(|m| m.to_vec()));
        self.cmds.push(argv);
        self
    }

    // ── Execution + escape hatch ──────────────────────────────────

    /// Send the batched commands as one write and drain one reply per
    /// command in declaration order. Single network round-trip.
    ///
    /// Per-command errors land as `Reply::Error(_)` entries in the
    /// returned vec; outer `Err` means a connection-level failure.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("INCR hits", ":1\r\n"), ("GET missing", "$-1\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// use kevy_client_async::pipeline::Pipeline;
    /// use kevy_resp::Reply;
    ///
    /// let replies = Pipeline::new().incr(b"hits").get(b"missing").run(&mut c).await?;
    /// assert_eq!(replies, [Reply::Int(1), Reply::Nil]);
    /// // an empty pipeline sends nothing
    /// assert!(Pipeline::new().run(&mut c).await?.is_empty());
    /// # Ok(()) }
    /// ```
    pub async fn run<T: crate::AsyncTransport>(
        self,
        conn: &mut AsyncConnection<T>,
    ) -> io::Result<Vec<Reply>> {
        if self.cmds.is_empty() {
            return Ok(Vec::new());
        }
        conn.codec_mut().pipeline(&self.cmds).await
    }

    /// Hand back the raw argv vectors so the same builder can drive a
    /// blocking client (or any other RESP transport). See module doc
    /// for the degrade-path pattern.
    ///
    /// ```
    /// use kevy_client_async::pipeline::Pipeline;
    ///
    /// let cmds = Pipeline::new().get(b"a").incr(b"b").into_cmds();
    /// assert_eq!(cmds, [vec![&b"GET"[..], b"a"], vec![&b"INCR"[..], b"b"]]);
    /// ```
    pub fn into_cmds(self) -> Vec<Vec<Vec<u8>>> {
        self.cmds
    }
}

// ── AsyncConnection entry point ───────────────────────────────────

impl<T: crate::AsyncTransport> AsyncConnection<T> {
    /// Open a [`Pipeline`] builder bound to this connection. Chain
    /// command-queue methods on the returned [`Pipeline`] then call
    /// `.run(&mut conn).await`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[("SET k v", "+OK\r\n"), ("GET k", "$1\r\nv\r\n")]).await?;
    /// use kevy_client_async::AsyncConnection;
    ///
    /// let mut c = AsyncConnection::connect(&format!("kevy://{addr}")).await?;
    /// let replies = c.pipeline().set(b"k", b"v").get(b"k").run(&mut c).await?;
    /// assert_eq!(replies.len(), 2);
    /// # Ok(()) }
    /// ```
    pub fn pipeline(&mut self) -> Pipeline {
        Pipeline::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_accumulates_in_order() {
        let p =
            Pipeline::new().set(b"k", b"v").get(b"k").incr(b"counter").del(&[&b"a"[..], &b"b"[..]]);
        assert_eq!(p.len(), 4);
        let cmds = p.into_cmds();
        assert_eq!(cmds[0][0], b"SET");
        assert_eq!(cmds[1][0], b"GET");
        assert_eq!(cmds[2][0], b"INCR");
        assert_eq!(cmds[3], vec![b"DEL".to_vec(), b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn empty_pipeline_yields_empty_cmds() {
        let p = Pipeline::new();
        assert!(p.is_empty());
        assert_eq!(p.into_cmds(), Vec::<Vec<Vec<u8>>>::new());
    }

    #[test]
    fn push_raw_escape_hatch() {
        let cmds = Pipeline::new().push_raw(vec![b"CUSTOM".to_vec(), b"arg".to_vec()]).into_cmds();
        assert_eq!(cmds, vec![vec![b"CUSTOM".to_vec(), b"arg".to_vec()]]);
    }
}
