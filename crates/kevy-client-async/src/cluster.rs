//! Async cluster-aware client: one connection per shard, CRC16
//! routing per key. Mirror of `kevy_client::ClusterClient`.
//!
//! Topology discovered once at connect via `CLUSTER SLOTS`; subsequent
//! key-routed commands go straight to the owner shard — `-MOVED` never
//! fires for correct routing.
//!
//! ```
//! # include!("doc_serve.rs");
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> std::io::Result<()> {
//! # let addr = serve(&[ONE_SHARD, ("SET user:1 ada", "+OK\r\n"), ("GET user:1", "$3\r\nada\r\n")]).await?;
//! use kevy_client_async::cluster::AsyncClusterClient;
//!
//! let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
//! // every key goes straight to the shard that owns its slot
//! c.set(b"user:1", b"ada").await?;
//! assert_eq!(c.get(b"user:1").await?.as_deref(), Some(&b"ada"[..]));
//! # Ok(()) }
//! ```

use std::io;
use std::time::Duration;

use kevy_hash::key_hash_slot;
use kevy_resp::Reply;

use crate::cluster_topology::{build_topology, parse_cluster_slots};
use crate::codec::AsyncRespCodec;
use crate::reply::{string, unexpected, vec2, vec3};

use crate::conn::{DefaultTransport, connect_default};
use crate::{AsyncSecure, AsyncTransport};

/// One open connection per distinct shard node + a slot→shard table.
///
/// The transport defaults to the runtime's `TcpStream`;
/// [`Self::connect_secure_url`] gives one over [`AsyncSecure`] instead.
///
/// ```
/// # include!("doc_serve.rs");
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
/// # let addr = serve(&[ONE_SHARD, ("INCR hits", ":1\r\n")]).await?;
/// use kevy_client_async::cluster::AsyncClusterClient;
///
/// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
/// assert_eq!(c.shard_count(), 1);
/// assert_eq!(c.incr(b"hits").await?, 1);
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct AsyncClusterClient<T = DefaultTransport> {
    shards: Vec<AsyncRespCodec<T>>,
    slot_to_shard: Vec<u16>,
}

/// `CLUSTER SLOTS` from the seed: each shard's address and the slot table.
async fn topology<T: AsyncTransport>(
    seed: &mut AsyncRespCodec<T>,
) -> io::Result<(Vec<(String, u16)>, Vec<u16>)> {
    let reply = seed.request(&[b"CLUSTER".to_vec(), b"SLOTS".to_vec()]).await?;
    build_topology(&parse_cluster_slots(reply)?)
}

impl AsyncClusterClient {
    /// Connect via a seed node, discover topology, open one connection
    /// per shard.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("PING", "+PONG\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// c.ping().await?; // any node of the cluster works as the seed
    /// # Ok(()) }
    /// ```
    pub async fn connect(host: &str, port: u16) -> io::Result<Self> {
        let mut seed = AsyncRespCodec::new(connect_default(host, port).await?);
        let (nodes, slot_to_shard) = topology(&mut seed).await?;
        let mut shards = Vec::with_capacity(nodes.len());
        for (h, p) in &nodes {
            shards.push(AsyncRespCodec::new(connect_default(h, *p).await?));
        }
        Ok(Self { shards, slot_to_shard })
    }
}

impl AsyncClusterClient<AsyncSecure<DefaultTransport>> {
    /// [`AsyncClusterClient::connect`] through encrypted cluster ports:
    /// `kevys://host:port?server_key=<hex>[&client_key_file=<path>]` names
    /// one of them, and every shard is reached through the encrypted port
    /// the server advertises, with the same keys.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let (addr, key) = serve_secure(&[ONE_SHARD, ("PING", "+PONG\r\n")]).await?;
    /// use kevy_client_async::cluster::AsyncClusterClient;
    /// # let key = hex(&key);
    /// let url = format!("kevys://{addr}?server_key={key}");
    /// let mut c = AsyncClusterClient::connect_secure_url(&url).await?; // seed and shard sealed
    /// c.ping().await?;
    /// # Ok(()) }
    /// ```
    pub async fn connect_secure_url(url: &str) -> io::Result<Self> {
        let u = kevy_resp_client::SecureUrl::parse(url)?;
        let me = u.client_key_file.as_deref().map(kevy_resp_client::load_client_key).transpose()?;
        let dial = |h: String, p: u16| {
            let me = me.clone();
            async move {
                let tcp = connect_default(&h, p).await?;
                AsyncSecure::handshake(tcp, u.server_key, me.as_ref()).await
            }
        };
        let mut seed = AsyncRespCodec::new(dial(u.host.clone(), u.port).await?);
        let (nodes, slot_to_shard) = topology(&mut seed).await?;
        let mut shards = Vec::with_capacity(nodes.len());
        for (h, p) in nodes {
            shards.push(AsyncRespCodec::new(dial(h, p).await?));
        }
        Ok(Self { shards, slot_to_shard })
    }
}

impl<T: AsyncTransport> AsyncClusterClient<T> {
    /// Number of distinct shard nodes.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert_eq!(c.shard_count(), 1); // one node owns all 16384 slots
    /// # Ok(()) }
    /// ```
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Route a single-key command to its owner shard.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("OBJECT ENCODING k", "$6\r\nembstr\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// use kevy_resp::Reply;
    ///
    /// let argv = [b"OBJECT".to_vec(), b"ENCODING".to_vec(), b"k".to_vec()];
    /// let reply = c.request_keyed(b"k", &argv).await?; // sent to the owner of `k`
    /// assert_eq!(reply, Reply::Bulk(b"embstr".to_vec()));
    /// # Ok(()) }
    /// ```
    pub async fn request_keyed(&mut self, key: &[u8], args: &[Vec<u8>]) -> io::Result<Reply> {
        let i = self.shard_for(key);
        self.shards[i].request(args).await
    }

    /// Keyless command — answered identically by any shard.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("TIME", "*2\r\n$10\r\n1700000000\r\n$1\r\n0\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// use kevy_resp::Reply;
    ///
    /// let Reply::Array(parts) = c.request_unkeyed(&[b"TIME".to_vec()]).await? else { panic!() };
    /// assert_eq!(parts.len(), 2);
    /// # Ok(()) }
    /// ```
    pub async fn request_unkeyed(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        self.shards[0].request(args).await
    }

    fn shard_for(&self, key: &[u8]) -> usize {
        self.slot_to_shard[key_hash_slot(key) as usize] as usize
    }

    /// `PING`. Answered by any shard.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("PING", "+PONG\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// c.ping().await?;
    /// # Ok(()) }
    /// ```
    pub async fn ping(&mut self) -> io::Result<()> {
        match self.request_unkeyed(&[b"PING".to_vec()]).await? {
            Reply::Simple(s) if s == b"PONG" || s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PUBLISH channel message`. Returns subscriber count.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("PUBLISH news hi", ":3\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert_eq!(c.publish(b"news", b"hi").await?, 3); // three subscribers
    /// # Ok(()) }
    /// ```
    pub async fn publish(&mut self, channel: &[u8], message: &[u8]) -> io::Result<usize> {
        match self.request_unkeyed(&vec3(b"PUBLISH", channel, message)).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SET key value`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("SET k v", "+OK\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// c.set(b"k", b"v").await?;
    /// # Ok(()) }
    /// ```
    pub async fn set(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        match self.request_keyed(key, &vec3(b"SET", key, value)).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SET key value PX ttl_ms`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("SET token abc PX 30000", "+OK\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// c.set_with_ttl(b"token", b"abc", std::time::Duration::from_secs(30)).await?;
    /// # Ok(()) }
    /// ```
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
        match self.request_keyed(key, &args).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `GET key`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("GET k", "$1\r\nv\r\n"), ("GET missing", "$-1\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert_eq!(c.get(b"k").await?.as_deref(), Some(&b"v"[..]));
    /// assert_eq!(c.get(b"missing").await?, None);
    /// # Ok(()) }
    /// ```
    pub async fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.request_keyed(key, &vec2(b"GET", key)).await? {
            Reply::Bulk(v) => Ok(Some(v)),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCR key`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("INCR hits", ":1\r\n"), ("INCR hits", ":2\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert_eq!(c.incr(b"hits").await?, 1);
    /// assert_eq!(c.incr(b"hits").await?, 2);
    /// # Ok(()) }
    /// ```
    pub async fn incr(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.request_keyed(key, &vec2(b"INCR", key)).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCRBY key delta`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("INCRBY stock -3", ":7\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert_eq!(c.incr_by(b"stock", -3).await?, 7);
    /// # Ok(()) }
    /// ```
    pub async fn incr_by(&mut self, key: &[u8], delta: i64) -> io::Result<i64> {
        let args = vec![b"INCRBY".to_vec(), key.to_vec(), delta.to_string().into_bytes()];
        match self.request_keyed(key, &args).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PEXPIRE key ttl_ms`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("PEXPIRE session 1500", ":1\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert!(c.expire(b"session", std::time::Duration::from_millis(1500)).await?);
    /// # Ok(()) }
    /// ```
    pub async fn expire(&mut self, key: &[u8], ttl: Duration) -> io::Result<bool> {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        let args = vec![b"PEXPIRE".to_vec(), key.to_vec(), ms.to_string().into_bytes()];
        match self.request_keyed(key, &args).await? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PERSIST key`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("PERSIST session", ":1\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert!(c.persist(b"session").await?); // the TTL is gone
    /// # Ok(()) }
    /// ```
    pub async fn persist(&mut self, key: &[u8]) -> io::Result<bool> {
        match self.request_keyed(key, &vec2(b"PERSIST", key)).await? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PTTL key`.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("PTTL session", ":1200\r\n"), ("PTTL missing", ":-2\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert_eq!(c.ttl_ms(b"session").await?, 1200);
    /// assert_eq!(c.ttl_ms(b"missing").await?, -2); // no key
    /// # Ok(()) }
    /// ```
    pub async fn ttl_ms(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.request_keyed(key, &vec2(b"PTTL", key)).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `DEL key [key ...]` — routed per key, summed.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("DEL a", ":1\r\n"), ("DEL b", ":0\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert_eq!(c.del(&[b"a", b"b"]).await?, 1); // one DEL per key; only `a` existed
    /// # Ok(()) }
    /// ```
    pub async fn del(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        let mut removed = 0;
        for k in keys {
            match self.request_keyed(k, &vec2(b"DEL", k)).await? {
                Reply::Int(n) if n >= 0 => removed += n as usize,
                Reply::Error(e) => return Err(io::Error::other(string(e))),
                other => return Err(unexpected(other)),
            }
        }
        Ok(removed)
    }

    /// `EXISTS key [key ...]` — routed per key, summed.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("EXISTS a", ":1\r\n"), ("EXISTS b", ":1\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert_eq!(c.exists(&[b"a", b"b"]).await?, 2);
    /// # Ok(()) }
    /// ```
    pub async fn exists(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        let mut count = 0;
        for k in keys {
            match self.request_keyed(k, &vec2(b"EXISTS", k)).await? {
                Reply::Int(n) if n >= 0 => count += n as usize,
                Reply::Error(e) => return Err(io::Error::other(string(e))),
                other => return Err(unexpected(other)),
            }
        }
        Ok(count)
    }

    /// `DBSIZE` — cluster-wide total (server fans out internally).
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("DBSIZE", ":42\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// assert_eq!(c.dbsize().await?, 42);
    /// # Ok(()) }
    /// ```
    pub async fn dbsize(&mut self) -> io::Result<usize> {
        match self.request_unkeyed(&[b"DBSIZE".to_vec()]).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `FLUSHALL` — clears every shard.
    ///
    /// ```
    /// # include!("doc_serve.rs");
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> std::io::Result<()> {
    /// # let addr = serve(&[ONE_SHARD, ("FLUSHALL", "+OK\r\n"), ("DBSIZE", ":0\r\n")]).await?;
    /// # use kevy_client_async::cluster::AsyncClusterClient;
    /// let mut c = AsyncClusterClient::connect("127.0.0.1", addr.port()).await?;
    /// c.flushall().await?;
    /// assert_eq!(c.dbsize().await?, 0);
    /// # Ok(()) }
    /// ```
    pub async fn flushall(&mut self) -> io::Result<()> {
        match self.request_unkeyed(&[b"FLUSHALL".to_vec()]).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}
