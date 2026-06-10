//! `CLUSTER` — the read-only single-node cluster surface.
//!
//! With `[cluster] enabled`, kevy presents each shard as a virtual master
//! "node" at `ip:(port_base + i)` owning one contiguous slot range, so stock
//! cluster-aware clients (`redis-benchmark --cluster`, `redis-cli -c`,
//! client libraries) discover the topology and connect per shard. Disabled
//! (default), every subcommand keeps the standalone stub shape clients
//! expect from a non-cluster Redis.
//!
//! Single-machine scope: no failover, no MIGRATE/ASK, no gossip — the
//! topology is static and fully derived from the config.

use std::cell::Cell;

use kevy_config::Config;
use kevy_resp::{ArgvView, encode_array_len, encode_bulk, encode_integer, encode_simple_string};
use kevy_store::Store;

use super::wrong_args;

thread_local! {
    /// This reactor thread's shard id (thread-per-core: thread == shard).
    /// Set by `KevyCommands::on_shard_start`; the `usize::MAX` sentinel
    /// (never a real shard) marks non-reactor contexts (tests, embedded).
    static CURRENT_SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// Record the current thread's shard id (see [`CURRENT_SHARD`]).
pub(crate) fn set_current_shard(shard: usize) {
    CURRENT_SHARD.with(|c| c.set(shard));
}

fn current_shard() -> usize {
    let s = CURRENT_SHARD.with(|c| c.get());
    if s == usize::MAX { 0 } else { s }
}

/// Deterministic 40-hex node id for shard `i` (stable across restarts;
/// `i + 1` so no id collides with the all-zero "unknown node" sentinel).
fn node_id(i: usize) -> String {
    format!("{:040x}", i + 1)
}

/// Advertised IPv4: the bind address, with `127.0.0.1` substituted for a
/// `0.0.0.0` wildcard (an unroutable advertise would strand every client).
fn advertised_ip(cfg: &Config) -> String {
    let [a, b, c, d] = cfg.server.bind;
    if [a, b, c, d] == [0, 0, 0, 0] {
        "127.0.0.1".into()
    } else {
        format!("{a}.{b}.{c}.{d}")
    }
}

pub(crate) fn cmd_cluster<A: ArgvView + ?Sized>(
    cfg: &Config,
    store: &mut Store,
    args: &A,
    out: &mut Vec<u8>,
) {
    let sub = match args.get(1) {
        Some(s) => s.to_ascii_uppercase(),
        None => return wrong_args(out, "cluster"),
    };
    let n = cfg.server.threads.max(1);
    let enabled = cfg.cluster.enabled;
    match sub.as_slice() {
        b"INFO" => {
            let body = format!(
                "cluster_enabled:{}\r\ncluster_state:ok\r\n\
                 cluster_slots_assigned:16384\r\ncluster_slots_ok:16384\r\n\
                 cluster_slots_pfail:0\r\ncluster_slots_fail:0\r\n\
                 cluster_known_nodes:{}\r\ncluster_size:{}\r\n\
                 cluster_current_epoch:0\r\ncluster_my_epoch:0\r\n",
                enabled as u8,
                if enabled { n } else { 1 },
                if enabled { n } else { 1 },
            );
            encode_bulk(out, body.as_bytes());
        }
        b"NODES" if enabled => encode_bulk(out, nodes_text(cfg, n).as_bytes()),
        b"NODES" => {
            // Standalone stub (matches the pre-cluster shape).
            let body = "0000000000000000000000000000000000000000 :0@0 myself,master - 0 0 0 connected 0-16383\r\n";
            encode_bulk(out, body.as_bytes());
        }
        b"SLOTS" if enabled => encode_slots(cfg, n, out),
        b"SLOTS" => encode_array_len(out, 0),
        b"SHARDS" if enabled => encode_shards(cfg, n, out),
        b"SHARDS" => encode_array_len(out, 0),
        b"MYID" if enabled => encode_bulk(out, node_id(current_shard()).as_bytes()),
        b"MYID" => encode_bulk(out, b"0000000000000000000000000000000000000000"),
        b"KEYSLOT" => match args.get(2) {
            Some(key) => encode_integer(out, kevy_hash::key_hash_slot(key) as i64),
            None => wrong_args(out, "cluster|keyslot"),
        },
        b"COUNTKEYSINSLOT" => {
            let Some(slot) = args
                .get(2)
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.parse::<u16>().ok())
                .filter(|&s| s < 16384)
            else {
                return wrong_args(out, "cluster|countkeysinslot");
            };
            // Counts this shard's keyspace only (Redis semantics: the
            // answering node's view). O(keys of shard); diagnostic-only.
            let mut count = 0i64;
            store.snapshot_each(|key, _, _| {
                if kevy_hash::key_hash_slot(key) == slot {
                    count += 1;
                }
            });
            encode_integer(out, count);
        }
        _ => encode_simple_string(out, "OK"),
    }
}

/// Walk the advertised topology — the single derivation (advertised IP,
/// per-shard port, slot range) all three emitters (`NODES` / `SLOTS` /
/// `SHARDS`) format from: `f(i, ip, port, start, end)` per virtual node.
fn for_each_node(cfg: &Config, n: usize, mut f: impl FnMut(usize, &str, i64, u16, u16)) {
    let ip = advertised_ip(cfg);
    let base = crate::cluster_port_base(cfg) as i64;
    for i in 0..n {
        let (start, end) = kevy_rt::shard_slot_range(i, n);
        f(i, &ip, base + i as i64, start, end);
    }
}

/// `CLUSTER NODES` text: one line per virtual node. The answering shard is
/// flagged `myself`. No cluster bus — `@cport` mirrors the data port.
fn nodes_text(cfg: &Config, n: usize) -> String {
    let me = current_shard();
    let mut body = String::new();
    for_each_node(cfg, n, |i, ip, port, start, end| {
        let flags = if i == me { "myself,master" } else { "master" };
        body.push_str(&format!(
            "{} {ip}:{port}@{port} {flags} - 0 0 {} connected {start}-{end}\r\n",
            node_id(i),
            i + 1,
        ));
    });
    body
}

/// `CLUSTER SLOTS`: `[[start, end, [ip, port, id, []]], …]` — the 4th node
/// element (metadata map, RESP2-encoded as an empty array) matches the
/// Redis 7 / valkey shape clients are parsed against.
fn encode_slots(cfg: &Config, n: usize, out: &mut Vec<u8>) {
    encode_array_len(out, n as i64);
    for_each_node(cfg, n, |i, ip, port, start, end| {
        encode_array_len(out, 3);
        encode_integer(out, start as i64);
        encode_integer(out, end as i64);
        encode_array_len(out, 4);
        encode_bulk(out, ip.as_bytes());
        encode_integer(out, port);
        encode_bulk(out, node_id(i).as_bytes());
        encode_array_len(out, 0);
    });
}

/// `CLUSTER SHARDS` (Redis 7 shape): per shard a 2-pair map-as-array of
/// `slots` `[start, end]` and `nodes` `[node-detail-map]`.
fn encode_shards(cfg: &Config, n: usize, out: &mut Vec<u8>) {
    encode_array_len(out, n as i64);
    for_each_node(cfg, n, |i, ip, port, start, end| {
        encode_array_len(out, 4); // 2 k/v pairs flattened
        encode_bulk(out, b"slots");
        encode_array_len(out, 2);
        encode_integer(out, start as i64);
        encode_integer(out, end as i64);
        encode_bulk(out, b"nodes");
        encode_array_len(out, 1);
        encode_array_len(out, 12); // 6 k/v pairs flattened
        encode_bulk(out, b"id");
        encode_bulk(out, node_id(i).as_bytes());
        encode_bulk(out, b"port");
        encode_integer(out, port);
        encode_bulk(out, b"ip");
        encode_bulk(out, ip.as_bytes());
        encode_bulk(out, b"endpoint");
        encode_bulk(out, ip.as_bytes());
        encode_bulk(out, b"role");
        encode_bulk(out, b"master");
        encode_bulk(out, b"health");
        encode_bulk(out, b"online");
    });
}
