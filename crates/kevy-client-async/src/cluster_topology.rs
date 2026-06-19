//! `CLUSTER SLOTS` reply parser + slot→shard topology builder.
//!
//! Pure (no IO) so the routing is unit-testable without a live server.
//! Same wire shape + same semantics as `kevy_client::cluster`:
//! `[[start, end, [host, port, id, [replicas...]], …], …]`.

use std::io;

use kevy_resp::Reply;

/// Redis-cluster keyspace size: every key hashes to one of these slots.
pub(crate) const NUM_SLOTS: usize = 16384;

/// `(distinct shard nodes in advertised order, slot → shard-index)`.
pub(crate) type Topology = (Vec<(String, u16)>, Vec<u16>);

/// One `[start, end, host, port]` entry parsed out of `CLUSTER SLOTS`.
pub(crate) struct SlotRange {
    pub start: u16,
    pub end: u16,
    pub host: String,
    pub port: u16,
}

pub(crate) fn parse_cluster_slots(reply: Reply) -> io::Result<Vec<SlotRange>> {
    let Reply::Array(rows) = reply else {
        return Err(bad());
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Reply::Array(cols) = row else {
            return Err(bad());
        };
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
        if !(0..=i64::from(u16::MAX)).contains(&start)
            || !(0..=i64::from(u16::MAX)).contains(&end)
            || !(0..=i64::from(u16::MAX)).contains(&port)
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

pub(crate) fn build_topology(ranges: &[SlotRange]) -> io::Result<Topology> {
    if ranges.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "CLUSTER SLOTS returned no ranges",
        ));
    }
    let mut nodes: Vec<(String, u16)> = Vec::new();
    let mut slot_to_shard = vec![0u16; NUM_SLOTS];
    for r in ranges {
        let idx = if let Some(i) = nodes.iter().position(|(h, p)| h == &r.host && *p == r.port) {
            i
        } else {
            nodes.push((r.host.clone(), r.port));
            nodes.len() - 1
        } as u16;
        for slot in r.start..=r.end {
            slot_to_shard[slot as usize] = idx;
        }
    }
    Ok((nodes, slot_to_shard))
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

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed CLUSTER SLOTS reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_hash::key_hash_slot;

    fn four_shard_reply() -> Reply {
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
        assert_eq!(slot_to_shard[0], 0);
        assert_eq!(slot_to_shard[4096], 1);
        assert_eq!(slot_to_shard[8192], 2);
        assert_eq!(slot_to_shard[16383], 3);
    }

    #[test]
    fn keys_route_to_their_slot_owner() {
        let ranges = parse_cluster_slots(four_shard_reply()).unwrap();
        let (_, slot_to_shard) = build_topology(&ranges).unwrap();
        for k in ["k0", "k1", "user:42", "rate:10.0.0.1", "gl:abc"] {
            let slot = key_hash_slot(k.as_bytes()) as usize;
            let shard = slot_to_shard[slot] as usize;
            assert_eq!(shard, slot / 4096, "key {k} slot {slot}");
        }
    }

    #[test]
    fn rejects_empty_and_malformed() {
        assert!(parse_cluster_slots(Reply::Int(1)).is_err());
        assert!(build_topology(&[]).is_err());
        assert!(
            parse_cluster_slots(Reply::Array(vec![Reply::Array(vec![Reply::Int(0)])])).is_err()
        );
    }
}
