//! Async cluster-aware client: one connection per shard, CRC16
//! routing per key. Mirror of `kevy_client::ClusterClient`.
//!
//! Topology discovered once at connect via `CLUSTER SLOTS`; subsequent
//! key-routed commands go straight to the owner shard — `-MOVED` never
//! fires for correct routing.

use std::io;
use std::time::Duration;

use kevy_hash::key_hash_slot;
use kevy_resp::Reply;

use crate::cluster_topology::{build_topology, parse_cluster_slots};
use crate::codec::AsyncRespCodec;
use crate::reply::{string, unexpected, vec2, vec3};

#[cfg(feature = "tokio")]
type DefaultTransport = tokio::net::TcpStream;
#[cfg(feature = "smol")]
type DefaultTransport = smol::net::TcpStream;
#[cfg(feature = "async-std")]
type DefaultTransport = async_std::net::TcpStream;

#[cfg(feature = "tokio")]
async fn connect_default(host: &str, port: u16) -> io::Result<DefaultTransport> {
    crate::rt_tokio::connect(host, port).await
}
#[cfg(feature = "smol")]
async fn connect_default(host: &str, port: u16) -> io::Result<DefaultTransport> {
    crate::rt_smol::connect(host, port).await
}
#[cfg(feature = "async-std")]
async fn connect_default(host: &str, port: u16) -> io::Result<DefaultTransport> {
    crate::rt_async_std::connect(host, port).await
}

/// One open connection per distinct shard node + a slot→shard table.
pub struct AsyncClusterClient {
    shards: Vec<AsyncRespCodec<DefaultTransport>>,
    slot_to_shard: Vec<u16>,
}

impl AsyncClusterClient {
    /// Connect via a seed node, discover topology, open one connection
    /// per shard.
    pub async fn connect(host: &str, port: u16) -> io::Result<Self> {
        let mut seed_codec = AsyncRespCodec::new(connect_default(host, port).await?);
        let reply = seed_codec
            .request(&[b"CLUSTER".to_vec(), b"SLOTS".to_vec()])
            .await?;
        let ranges = parse_cluster_slots(reply)?;
        let (nodes, slot_to_shard) = build_topology(&ranges)?;

        let mut shards = Vec::with_capacity(nodes.len());
        for (h, p) in &nodes {
            let transport = connect_default(h, *p).await?;
            shards.push(AsyncRespCodec::new(transport));
        }
        Ok(Self {
            shards,
            slot_to_shard,
        })
    }

    /// Number of distinct shard nodes.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Route a single-key command to its owner shard.
    pub async fn request_keyed(
        &mut self,
        key: &[u8],
        args: &[Vec<u8>],
    ) -> io::Result<Reply> {
        let i = self.shard_for(key);
        self.shards[i].request(args).await
    }

    /// Keyless command — answered identically by any shard.
    pub async fn request_unkeyed(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        self.shards[0].request(args).await
    }

    fn shard_for(&self, key: &[u8]) -> usize {
        self.slot_to_shard[key_hash_slot(key) as usize] as usize
    }

    /// `PING`. Answered by any shard.
    pub async fn ping(&mut self) -> io::Result<()> {
        match self.request_unkeyed(&[b"PING".to_vec()]).await? {
            Reply::Simple(s) if s == b"PONG" || s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PUBLISH channel message`. Returns subscriber count.
    pub async fn publish(&mut self, channel: &[u8], message: &[u8]) -> io::Result<usize> {
        match self
            .request_unkeyed(&vec3(b"PUBLISH", channel, message))
            .await?
        {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SET key value`.
    pub async fn set(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        match self.request_keyed(key, &vec3(b"SET", key, value)).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SET key value PX ttl_ms`.
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
    pub async fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.request_keyed(key, &vec2(b"GET", key)).await? {
            Reply::Bulk(v) => Ok(Some(v)),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCR key`.
    pub async fn incr(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.request_keyed(key, &vec2(b"INCR", key)).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCRBY key delta`.
    pub async fn incr_by(&mut self, key: &[u8], delta: i64) -> io::Result<i64> {
        let args = vec![
            b"INCRBY".to_vec(),
            key.to_vec(),
            delta.to_string().into_bytes(),
        ];
        match self.request_keyed(key, &args).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PEXPIRE key ttl_ms`.
    pub async fn expire(&mut self, key: &[u8], ttl: Duration) -> io::Result<bool> {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        let args = vec![
            b"PEXPIRE".to_vec(),
            key.to_vec(),
            ms.to_string().into_bytes(),
        ];
        match self.request_keyed(key, &args).await? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PERSIST key`.
    pub async fn persist(&mut self, key: &[u8]) -> io::Result<bool> {
        match self.request_keyed(key, &vec2(b"PERSIST", key)).await? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PTTL key`.
    pub async fn ttl_ms(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.request_keyed(key, &vec2(b"PTTL", key)).await? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `DEL key [key ...]` — routed per key, summed.
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
    pub async fn dbsize(&mut self) -> io::Result<usize> {
        match self.request_unkeyed(&[b"DBSIZE".to_vec()]).await? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `FLUSHALL` — clears every shard.
    pub async fn flushall(&mut self) -> io::Result<()> {
        match self.request_unkeyed(&[b"FLUSHALL".to_vec()]).await? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}
