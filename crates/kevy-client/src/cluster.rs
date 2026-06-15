//! Cluster-aware client: one connection per shard, routing each key to its
//! owner shard by CRC16 slot — so no command pays the server-side cross-shard
//! forwarding hop (the low-load tail / high-load throughput cost measured on
//! the kevy-server role). The topology is discovered once at connect via
//! `CLUSTER SLOTS`, which advertises each shard's slot range + address, so the
//! client routes against the server's *actual* partition (no need to replicate
//! its `slot → shard` arithmetic).
//!
//! Requires the server to run in cluster mode (`--cluster`): each shard then
//! binds a deterministic listener and answers a wrong-shard key with `-MOVED`
//! rather than forwarding. Correct routing here means `-MOVED` never fires.

use std::io;
use std::time::Duration;

use kevy_hash::key_hash_slot;
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

use crate::{string, unexpected, vec2, vec3};

/// Redis-cluster keyspace size: every key hashes to one of these slots.
const NUM_SLOTS: usize = 16384;

/// One open connection per distinct shard node, with a slot → shard index so a
/// single-key command goes straight to its owner.
pub struct ClusterClient {
    /// Per distinct shard node, in first-advertised order.
    shards: Vec<RespClient>,
    /// `slot_to_shard[slot]` = index into [`Self::shards`]. Length [`NUM_SLOTS`].
    slot_to_shard: Vec<u16>,
}

/// `(distinct shard nodes in advertised order, slot → shard-index)`.
type Topology = (Vec<(String, u16)>, Vec<u16>);

/// One `[start, end, host, port]` entry parsed out of `CLUSTER SLOTS`.
struct SlotRange {
    start: u16,
    end: u16,
    host: String,
    port: u16,
}

impl ClusterClient {
    /// Connect via a seed node, discover the topology (`CLUSTER SLOTS`), and
    /// open one connection per shard.
    pub fn connect(host: &str, port: u16) -> io::Result<Self> {
        let mut seed = RespClient::connect(host, port)?;
        let reply = seed.request(&[b"CLUSTER".to_vec(), b"SLOTS".to_vec()])?;
        let ranges = parse_cluster_slots(reply)?;
        let (nodes, slot_to_shard) = build_topology(&ranges)?;
        let shards = nodes
            .iter()
            .map(|(h, p)| RespClient::connect(h, *p))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { shards, slot_to_shard })
    }

    /// Owner-shard index of `key`.
    #[inline]
    fn shard_for(&self, key: &[u8]) -> usize {
        self.slot_to_shard[key_hash_slot(key) as usize] as usize
    }

    /// The connection to `key`'s owner shard — for callers (collection ops in
    /// `cluster_coll`) that build a request via a shared helper.
    #[inline]
    pub(crate) fn route_mut(&mut self, key: &[u8]) -> &mut RespClient {
        let i = self.shard_for(key);
        &mut self.shards[i]
    }

    /// Route a single-key command (`args`) to the shard owning `key`.
    pub fn request_keyed(&mut self, key: &[u8], args: &[Vec<u8>]) -> io::Result<Reply> {
        let i = self.shard_for(key);
        self.shards[i].request(args)
    }

    /// A keyless command (PING / etc.) — answered identically by any shard, so
    /// send it to the first.
    pub fn request_unkeyed(&mut self, args: &[Vec<u8>]) -> io::Result<Reply> {
        self.shards[0].request(args)
    }

    /// Number of distinct shard nodes.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

}

// ───────────── command surface (routed) ─────────────
//
// One connection per shard, so each single-key command goes straight to its
// owner — no `-MOVED`, no server forwarding hop. Multi-key DEL/EXISTS route
// per key and sum; keyspace-wide DBSIZE/FLUSHALL fan out to every shard.

impl ClusterClient {
    /// `PING` — answered by any shard.
    pub fn ping(&mut self) -> io::Result<()> {
        match self.request_unkeyed(&[b"PING".to_vec()])? {
            Reply::Simple(s) if s == b"PONG" || s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SET key value`.
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        match self.request_keyed(key, &vec3(b"SET", key, value))? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `SET key value PX ttl_ms` — value with an expiry.
    pub fn set_with_ttl(&mut self, key: &[u8], value: &[u8], ttl: Duration) -> io::Result<()> {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        let args = vec![
            b"SET".to_vec(),
            key.to_vec(),
            value.to_vec(),
            b"PX".to_vec(),
            ms.to_string().into_bytes(),
        ];
        match self.request_keyed(key, &args)? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `GET key`. `None` if absent or expired.
    pub fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.request_keyed(key, &vec2(b"GET", key))? {
            Reply::Bulk(v) => Ok(Some(v)),
            Reply::Nil => Ok(None),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCR key`. Returns the post-increment value.
    pub fn incr(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.request_keyed(key, &vec2(b"INCR", key))? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `INCRBY key delta`.
    pub fn incr_by(&mut self, key: &[u8], delta: i64) -> io::Result<i64> {
        let args = vec![b"INCRBY".to_vec(), key.to_vec(), delta.to_string().into_bytes()];
        match self.request_keyed(key, &args)? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PEXPIRE key ttl_ms`. Whether the key existed and got a TTL.
    pub fn expire(&mut self, key: &[u8], ttl: Duration) -> io::Result<bool> {
        let ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        let args = vec![b"PEXPIRE".to_vec(), key.to_vec(), ms.to_string().into_bytes()];
        match self.request_keyed(key, &args)? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PERSIST key`. Whether a TTL was removed.
    pub fn persist(&mut self, key: &[u8]) -> io::Result<bool> {
        match self.request_keyed(key, &vec2(b"PERSIST", key))? {
            Reply::Int(1) => Ok(true),
            Reply::Int(0) => Ok(false),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `PTTL key`. ms remaining, -2 no key, -1 no TTL.
    pub fn ttl_ms(&mut self, key: &[u8]) -> io::Result<i64> {
        match self.request_keyed(key, &vec2(b"PTTL", key))? {
            Reply::Int(n) => Ok(n),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `DEL key [key ...]` — routed per key (each to its owner) and summed, so
    /// keys spanning shards work without a same-slot constraint.
    pub fn del(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        let mut removed = 0;
        for k in keys {
            match self.request_keyed(k, &vec2(b"DEL", k))? {
                Reply::Int(n) if n >= 0 => removed += n as usize,
                Reply::Error(e) => return Err(io::Error::other(string(e))),
                other => return Err(unexpected(other)),
            }
        }
        Ok(removed)
    }

    /// `EXISTS key [key ...]` — routed per key and summed (a repeated key
    /// counts each time, matching Redis).
    pub fn exists(&mut self, keys: &[&[u8]]) -> io::Result<usize> {
        let mut count = 0;
        for k in keys {
            match self.request_keyed(k, &vec2(b"EXISTS", k))? {
                Reply::Int(n) if n >= 0 => count += n as usize,
                Reply::Error(e) => return Err(io::Error::other(string(e))),
                other => return Err(unexpected(other)),
            }
        }
        Ok(count)
    }

    /// `DBSIZE` — the cluster-wide total. kevy answers DBSIZE by fanning out
    /// across shards internally (`Route::Dbsize`), so a single shard already
    /// reports the whole-cluster count; no client-side summing.
    pub fn dbsize(&mut self) -> io::Result<usize> {
        match self.request_unkeyed(&[b"DBSIZE".to_vec()])? {
            Reply::Int(n) if n >= 0 => Ok(n as usize),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }

    /// `FLUSHALL` — clears every shard. kevy fans FLUSHALL out internally
    /// (`Route::Flush`), so one call wipes the whole cluster.
    pub fn flushall(&mut self) -> io::Result<()> {
        match self.request_unkeyed(&[b"FLUSHALL".to_vec()])? {
            Reply::Simple(s) if s == b"OK" => Ok(()),
            Reply::Error(e) => Err(io::Error::other(string(e))),
            other => Err(unexpected(other)),
        }
    }
}

/// Build the `(distinct nodes, slot → shard-index)` topology from the parsed
/// ranges. Distinct nodes are kept in first-advertised order; a slot left
/// uncovered (a gap a healthy cluster never has) defaults to shard 0. Pure —
/// no I/O — so the routing is unit-testable without a live server.
fn build_topology(ranges: &[SlotRange]) -> io::Result<Topology> {
    if ranges.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CLUSTER SLOTS returned no ranges",
        ));
    }
    let mut nodes: Vec<(String, u16)> = Vec::new();
    let mut slot_to_shard = vec![0u16; NUM_SLOTS];
    for r in ranges {
        let idx = match nodes.iter().position(|(h, p)| h == &r.host && *p == r.port) {
            Some(i) => i,
            None => {
                nodes.push((r.host.clone(), r.port));
                nodes.len() - 1
            }
        } as u16;
        for slot in r.start..=r.end {
            slot_to_shard[slot as usize] = idx;
        }
    }
    Ok((nodes, slot_to_shard))
}

/// Parse a `CLUSTER SLOTS` reply: `[[start, end, [host, port, id, []], …], …]`.
fn parse_cluster_slots(reply: Reply) -> io::Result<Vec<SlotRange>> {
    fn bad() -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, "malformed CLUSTER SLOTS reply")
    }
    let Reply::Array(rows) = reply else {
        return Err(bad());
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Reply::Array(cols) = row else { return Err(bad()) };
        if cols.len() < 3 {
            return Err(bad());
        }
        let start = as_int(&cols[0]).ok_or_else(bad)?;
        let end = as_int(&cols[1]).ok_or_else(bad)?;
        let Reply::Array(node) = &cols[2] else {
            return Err(bad());
        };
        if node.len() < 2 {
            return Err(bad());
        }
        let host = as_str(&node[0]).ok_or_else(bad)?;
        let port = as_int(&node[1]).ok_or_else(bad)?;
        if !(0..=u16::MAX as i64).contains(&start)
            || !(0..=u16::MAX as i64).contains(&end)
            || !(0..=u16::MAX as i64).contains(&port)
        {
            return Err(bad());
        }
        out.push(SlotRange {
            start: start as u16,
            end: end as u16,
            host,
            port: port as u16,
        });
    }
    Ok(out)
}

fn as_int(r: &Reply) -> Option<i64> {
    match r {
        Reply::Int(n) => Some(*n),
        _ => None,
    }
}

fn as_str(r: &Reply) -> Option<String> {
    match r {
        Reply::Bulk(b) | Reply::Simple(b) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4-shard even split mirroring the server's contiguous slot ranges,
    /// encoded as a `CLUSTER SLOTS` reply, then parsed + routed.
    fn four_shard_reply() -> Reply {
        // 16384 / 4 = 4096 slots each.
        let row = |start: i64, end: i64, port: i64| {
            Reply::Array(vec![
                Reply::Int(start),
                Reply::Int(end),
                Reply::Array(vec![
                    Reply::Bulk(b"127.0.0.1".to_vec()),
                    Reply::Int(port),
                    Reply::Bulk(b"nodeid".to_vec()),
                    Reply::Array(vec![]),
                ]),
            ])
        };
        Reply::Array(vec![
            row(0, 4095, 7001),
            row(4096, 8191, 7002),
            row(8192, 12287, 7003),
            row(12288, 16383, 7004),
        ])
    }

    #[test]
    fn parse_and_build_4_shard_topology() {
        let ranges = parse_cluster_slots(four_shard_reply()).unwrap();
        assert_eq!(ranges.len(), 4);
        let (nodes, slot_to_shard) = build_topology(&ranges).unwrap();
        assert_eq!(nodes.len(), 4);
        assert_eq!(nodes[0], ("127.0.0.1".to_string(), 7001));
        assert_eq!(nodes[3], ("127.0.0.1".to_string(), 7004));
        // Every slot maps to the advertised shard.
        assert_eq!(slot_to_shard[0], 0);
        assert_eq!(slot_to_shard[4095], 0);
        assert_eq!(slot_to_shard[4096], 1);
        assert_eq!(slot_to_shard[8192], 2);
        assert_eq!(slot_to_shard[16383], 3);
    }

    #[test]
    fn keys_route_to_their_slot_owner() {
        let ranges = parse_cluster_slots(four_shard_reply()).unwrap();
        let (_, slot_to_shard) = build_topology(&ranges).unwrap();
        // A key's shard = its CRC16 slot's owner — the same mapping the server
        // enforces, so `-MOVED` never fires.
        for k in ["k0", "k1", "user:42", "rate:10.0.0.1", "gl:abc"] {
            let slot = key_hash_slot(k.as_bytes()) as usize;
            let shard = slot_to_shard[slot] as usize;
            // Sanity: slot's owner is the contiguous-range shard.
            assert_eq!(shard, slot / 4096, "key {k} slot {slot}");
        }
    }

    #[test]
    fn rejects_empty_and_malformed() {
        assert!(parse_cluster_slots(Reply::Int(1)).is_err());
        assert!(build_topology(&[]).is_err());
        assert!(parse_cluster_slots(Reply::Array(vec![Reply::Array(vec![Reply::Int(0)])])).is_err());
    }
}
