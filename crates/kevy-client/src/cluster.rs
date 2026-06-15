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

use kevy_hash::key_hash_slot;
use kevy_resp::Reply;
use kevy_resp_client::RespClient;

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
