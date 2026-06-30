# Cluster

kevy's cluster surface has two independent layers — **single-node multi-shard exposure** (one process, every shard speaks Redis Cluster) and **multi-node replication + scoped multi-writer** (primaries, replicas, embeds, quorum failover) — and you can run either, both, or neither.

## The two layers at a glance

**Single-node cluster mode.** One kevy process partitions its keyspace across N shards and exposes each shard as a virtual cluster node on a deterministic per-shard port. `CLUSTER SLOTS / SHARDS / NODES` report the real CRC16 partition; key-aware clients (`redis-cli -c`, `redis-benchmark --cluster`, stock cluster-aware libraries, and the bundled [`ClusterClient`](https://github.com/goliajp/kevy/blob/master/crates/kevy-client/examples/cluster.rs)) hash each key and connect straight to the owning shard. The win is mechanical — removing the server-side cross-shard hop translates directly into higher throughput and lower tail latency.

**Multi-node cluster.** A kevy server can act as a **primary** streaming a write log to one or more **replicas** (either kevy servers or in-process [`kevy-embedded`](https://github.com/goliajp/kevy/tree/master/crates/kevy-embedded) stores). A primary can also delegate **scoped writes** by prefix: `[cluster] scopes` declares which node owns writes for `app:billing:*`, which owns `app:auth:*`, etc.; writes that land on the wrong node receive `-MISDIRECTED writer is <host:port>` so the client follows. [`kevy-elect`](https://github.com/goliajp/kevy/tree/master/crates/kevy-elect) provides a quorum heartbeat that flags a writer DOWN and promotes the declared fallback. Operator-issued `MOVE-SCOPE` migrates a prefix under a quiesce window.

## When you need this

| Situation | Reach for |
|-----------|-----------|
| One process, key-aware client, want the cross-shard hop gone | Single-node cluster mode + `ClusterClient` |
| Compatibility with stock Redis Cluster tooling on a single host | Single-node cluster mode |
| Hot reads served from another machine or in-process | Multi-node: primary + replicas (or embed-as-replica) |
| Multiple writers, partitioned by key prefix, on different hosts | Multi-node: scoped multi-writer |
| Surviving a writer crash without a human in the loop | Multi-node: `kevy-elect` + scope fallback |
| One process, low load, ordinary clients | Neither — the default proxy port is enough |

The two layers compose: a primary in cluster mode advertises N shards, each replica also runs N shards, and a routing client wires them together.

---

# Layer 1 — Single-node cluster mode

## Core idea

A normal kevy process accepts every command on a single port and internally forwards mis-routed keys to the shard that owns them. That forward is correct, but on a hot path it dominates p99 latency and caps throughput. Cluster mode exposes each shard at its own port; a key-aware client hashes the key with CRC16-XMODEM, looks up the owner shard from `CLUSTER SLOTS`, and connects straight there — no forward, no `-MOVED`.

```
                  ┌─────────────────────────────────────────┐
                  │            kevy process (1 host)        │
                  │                                         │
  main port  ───▶ │  6004  ── proxy: forwards or -MOVED ──▶ │
                  │                                         │
  shard ports ──▶ │  6005  ── shard 0   (slots     0– 4095) │
                  │  6006  ── shard 1   (slots  4096– 8191) │
                  │  6007  ── shard 2   (slots  8192–12287) │
                  │  6008  ── shard 3   (slots 12288–16383) │
                  └─────────────────────────────────────────┘
```

Shard `i` always binds `port_base + 1 + i` (override `port_base` via TOML). The main port keeps the proxy behaviour for clients that don't speak cluster; per-shard ports answer `-MOVED <slot> <host:port>` when a key arrives at the wrong owner.

Whole-keyspace commands (`KEYS`, `SCAN`, `DBSIZE`, `FLUSHALL`) stay whole-keyspace on every port — kevy fans them out internally so a client doesn't have to.

## Enabling it

```toml
# kevy.toml
port = 6004

[cluster]
enabled   = true
# port_base = 6004   # defaults to `port`; shards live at port_base + 1 + i
```

Equivalent CLI / env:

```sh
kevy --port 6004 --threads 8 --cluster      # shard ports 6005..6012
KEVY_CLUSTER=1 kevy --port 6004 --threads 8
```

Switching a data directory in or out of cluster mode re-homes the keys once at startup; the prior files are backed up as `*.premigration.<ts>`.

## Using `ClusterClient` from Rust

```toml
[dependencies]
kevy-client = "*"
```

```rust
use kevy_client::ClusterClient;

// Seed against any cluster port; topology is discovered via CLUSTER SLOTS
// and one connection is opened per shard.
let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;

cc.set(b"user:42", b"alice")?;
let v = cc.get(b"user:42")?;            // routed to user:42's owner shard
let n = cc.incr(b"counter")?;

// Multi-key DEL/EXISTS — routed per key and summed.
let removed = cc.del(&[b"a", b"b", b"c"])?;
# Ok::<(), std::io::Error>(())
```

A runnable seed example lives at [`crates/kevy-client/examples/cluster.rs`](https://github.com/goliajp/kevy/blob/master/crates/kevy-client/examples/cluster.rs); a benchmark at [`crates/kevy-client/examples/cluster_bench.rs`](https://github.com/goliajp/kevy/blob/master/crates/kevy-client/examples/cluster_bench.rs).

### How routing removes the cross-shard hop

1. **Discover.** `connect` sends `CLUSTER SLOTS` to the seed, reads each shard's `[start, end, host, port]`, and builds a 16384-entry `slot → shard-index` table. The table comes from the server's advertised ranges, so the client never reimplements the partitioning arithmetic.
2. **Route.** Every single-key command computes `key_hash_slot(key)` (CRC16-XMODEM over the `{hashtag}` if present, else the whole key) and sends straight to that slot's owner connection.
3. **Fan-out where needed.** `dbsize`, `flushall`, and other whole-cluster commands are handled server-side; the client issues one call.

On a 16-core lx64 box with GET at concurrency 64, removing the cross-shard hop lifts measured throughput from 333 k ops/s to 533 k ops/s (1.6×) and drops p99 from 3858 µs to 260 µs (~15× lower tail). Reproduce with `cargo run -p kevy-client --release --example cluster_bench`.

> The hop's cost only shows up under load on a clean machine. On a small co-located cloud VM the difference is buried in scheduling noise.

### Cross-slot multi-key commands

Unlike Redis Cluster, kevy does **not** return `-CROSSSLOT` when a multi-key command (`MGET`, `MSET`, `SUNION`, transactions, blocking fan-outs) spans shards on a single-node cluster: the server fulfils the request across shards. kevy is a superset of Redis Cluster on a single machine — every Redis Cluster client works, plus the surface you would have hit `-CROSSSLOT` on still works. A shared `{hashtag}` is still the right tool when you need data co-located for atomicity, but it is no longer required for correctness.

### `CLUSTER` commands supported on a cluster port

| Command | Behaviour |
|---------|-----------|
| `CLUSTER SLOTS` | Real partition: one `[start, end, host, port]` row per shard. |
| `CLUSTER SHARDS` | Newer shape of the same data, primary nodes only. |
| `CLUSTER NODES` | Flat text manifest, one row per shard, IDs derived from shard index. |
| `CLUSTER MYID` | Deterministic ID for the shard answering the call. |
| `CLUSTER KEYSLOT <key>` | CRC16-XMODEM over the `{hashtag}` or whole key. |
| `CLUSTER COUNTKEYSINSLOT <slot>` | Live count by walking the owning shard's index. |
| `CLUSTER COUNT-FAILURE-REPORTS <id>` | Always 0 — there is no failure detector on this layer. |
| `CLUSTER INFO` | Reports `cluster_enabled:1`, `cluster_state:ok`, slot coverage. |
| `CLUSTER RESET`, `CLUSTER FORGET`, `CLUSTER MEET`, `CLUSTER FAILOVER`, `MIGRATE`, `ASK` | Not implemented — see *Out of scope*. |

### Falling back to raw routed helpers

```rust
// Route an arbitrary single-key command to its owner shard.
let reply = cc.request_keyed(b"mykey", &[b"STRLEN".to_vec(), b"mykey".to_vec()])?;
// Keyless commands go to any shard.
let reply = cc.request_unkeyed(&[b"PING".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

`ClusterClient` wraps the common verbs across strings, hashes, lists, sets, sorted sets, pub/sub, and the multi-key `DEL` / `EXISTS`. Pub/sub is process-global: a `Subscriber` on any port sees every published message regardless of which shard accepted the `PUBLISH`.

---

# Layer 2 — Multi-node cluster

## Primary and replicas

A kevy server can be a primary (default), a replica that mirrors a primary's write log, or both at once (cascade). The primary opens a dedicated replication listener; replicas connect, hand over their last-applied offset, and apply the streamed frames onto local shards.

```toml
# primary.toml
port = 6004

[replication]
listen_port = 16004        # primary streams the log here
```

```toml
# replica.toml
port = 6004

[replication]
upstream    = "primary.local:16004"
replica_id  = "replica-eu-1"           # stable per replica; survives restarts
# reconnect_min_ms = 100               # backoff envelope
# reconnect_max_ms = 5000
```

Full server-side semantics — backlog sizing, snapshot ingest, cascade — live in [`docs/replication.md`](https://github.com/goliajp/kevy/blob/master/docs/replication.md). The relevant fact for this document is that the same wire protocol carries cluster-mode replication: a primary running with `[cluster] enabled = true` streams N shards' worth of writes, and a replica running with the same shard count applies them shard-for-shard.

## Embed as read-replica

A [`kevy-embedded`](https://github.com/goliajp/kevy/tree/master/crates/kevy-embedded) store can subscribe to a primary directly and serve in-process reads with zero network hop. Writes are refused locally with `READONLY`.

```rust
use kevy_embedded::Store;

// In-memory replica, AOF off, default reconnect (100 ms → 5 s).
let replica = Store::open_replica("primary.local:16004")?;

let v = replica.get(b"hello")?;
assert!(replica.set(b"k", b"v").is_err());      // READONLY
# Ok::<(), std::io::Error>(())
```

For tuning:

```rust
use std::time::Duration;
use kevy_embedded::{Config, Store};

let cfg = Config::default()
    .with_replica_upstream("primary.local:16004")
    .with_replica_id("backup-svc-region-a")
    .with_replica_reconnect(Duration::from_millis(50), Duration::from_secs(10));
let replica = Store::open(cfg)?;
# Ok::<(), std::io::Error>(())
```

The handshake sends `REPLICATE FROM <last-applied-offset> ID <replica_id>`; the primary acks the offset and streams frames. The runner thread is joined when the last `Store` clone drops, so the primary observes a clean FIN and frees the slot. `PUBLISH` is allowed locally on the embed (pub/sub is process-local), but the keyspace itself remains read-only.

## Scoped multi-writer

Scoped multi-writer splits writes by key prefix across nodes. Every node knows the full ownership table from static config; writes that land on a non-owner answer `-MISDIRECTED writer is <host:port>` so the client retries against the right node.

```toml
# Same config block on every member.
[cluster]
node_id = "embed-billing-1"
peers   = "embed-billing-1@10.0.0.1:6004,server-eu-1@10.0.0.2:6004,reader-1@10.0.0.3:6004"

# prefix=writer[|fallback], comma-separated.
# The first `=` splits prefix from owner spec, so `app:billing:` (with `:`) is fine.
scopes  = "app:billing:=embed-billing-1|server-eu-1, app:auth:=embed-auth-1"

elect_port_base = 16100    # kevy-elect listens here
```

`peers` is a flat string of `<node_id>@<host>:<port>` entries — no nested structure, easy to template. `scopes` is parsed `prefix=writer[|fallback]`, comma-separated. A node with no scope ownership simply forwards writes; a node that does own a scope accepts writes for it and rejects others.

Reads are independent of scope ownership — any node holding the data (typically a read-replica) can serve them. The scope mechanism is for write attribution only.

### Embed as scoped writer

```rust
use kevy_embedded::{Config, Store};

let writer = Store::open(
    Config::default().with_embed_writer("0.0.0.0:6105")
)?;

// Local writes feed the embed's replication source backlog;
// readers connect to 0.0.0.0:6105 via kevy_replicate::ReplicaClient.
writer.set(b"app:billing:invoice:42", b"...")?;
# Ok::<(), std::io::Error>(())
```

The embed exposes a replication listener on the address passed to `with_embed_writer`. Other nodes pull the log from there exactly as they would from a server primary.

## `kevy-elect` quorum failover

`kevy-elect` is a sidecar heartbeat that every cluster member runs. Each node ships an HB on the elect port (`elect_port_base + node_index`); each node maintains a sliding window of who has been alive recently. When a peer's last HB falls past `down_after` (default 5 s), it enters `down_peers`. A scope's declared fallback consults `down_peers` on every accepted write: if its writer is DOWN, the fallback treats itself as the active owner and accepts the write; the next write on every other node now MISDIRECTs to the fallback. When the original writer's HBs resume, it leaves `down_peers` and the fallback steps down implicitly on the next decision.

| Knob | Meaning | Default |
|------|---------|---------|
| `node_id` | This node's stable identifier (`<scope_owner>` references match it) | required |
| `peers` | `<node_id>@<host>:<port>` list of every cluster member | required |
| `elect_port_base` | UDP port the local elect sidecar binds | `16100` |
| `hb_interval_ms` | HB emit cadence | `500` |
| `down_after_ms` | Time without HB before a peer is DOWN | `5000` |

### Manual rejoin recovery

If the original writer was DOWN long enough that the fallback accepted writes, those writes only live on the fallback. Before re-enabling the original writer for the scope: stop the writer, copy the fallback's data directory into the writer's, then restart. This stays inside the no-consensus contract — no shadow-writes, no double-acceptance.

---

# `MOVE-SCOPE`

`MOVE-SCOPE` migrates a prefix from one writer to another under a bounded quiesce window. It is operator-issued and runs on the current writer.

```
MOVE-SCOPE <prefix> from <from-node-id> to <to-node-id>
```

Step by step:

1. The current writer flips local state for `<prefix>` to MIGRATING. Subsequent writes to keys under the prefix return `-QUIESCED migrating to <to-host:port>`. Clients back off briefly and retry.
2. The writer serialises the prefix's keyspace slice and ships it via `MOVE-SCOPE-INGEST <prefix> <bulk>` to the target's data port.
3. On `+OK` from the target, the writer commits the migration locally. Future writes for the prefix on the source now return `-MISDIRECTED writer is <to-host:port>`.
4. Other cluster members continue routing per their static `scopes` config until the operator pushes new config and restarts.

The two wire replies you will see during a move:

| Reply | Meaning |
|-------|---------|
| `-MISDIRECTED writer is <host:port>` | Write landed on a non-owner. Retry against the named host. |
| `-QUIESCED migrating to <host:port>` | Transient during a MOVE-SCOPE window. Back off and retry. |

A cluster-aware client caches the per-key target on `-MISDIRECTED` and retries transparently; on `-QUIESCED` it should sleep briefly (single-digit hundreds of milliseconds) before retrying.

Aborting mid-ship reverts to the source writer; no partial-apply state is left on the target.

---

# Configuration reference

## Single-node cluster mode

| TOML | CLI | Env | Default | Meaning |
|------|-----|-----|---------|---------|
| `[cluster] enabled` | `--cluster` | `KEVY_CLUSTER=1` | `false` | Expose each shard at a per-shard port. |
| `[cluster] port_base` | `--cluster-port-base` | `KEVY_CLUSTER_PORT_BASE` | value of `port` | Shard `i` binds `port_base + 1 + i`. |

## Replication (primary side)

| TOML | CLI | Env | Default |
|------|-----|-----|---------|
| `[replication] listen_port` | `--replication-listener` | `KEVY_REPLICATION_LISTEN_PORT` | unset (off) |

## Replication (replica side)

| TOML | CLI | Env | Default |
|------|-----|-----|---------|
| `[replication] upstream` | `--replicate-from` | `KEVY_REPLICATE_FROM` | unset |
| `[replication] replica_id` | `--replica-id` | `KEVY_REPLICA_ID` | derived from hostname |
| `[replication] reconnect_min_ms` | | | `100` |
| `[replication] reconnect_max_ms` | | | `5000` |

## Scoped multi-writer + elect

| TOML | Meaning |
|------|---------|
| `[cluster] node_id` | This node's stable identifier. |
| `[cluster] peers` | `<node_id>@<host>:<port>` list of every cluster member. |
| `[cluster] scopes` | `prefix=writer[\|fallback]` entries, comma-separated. |
| `[cluster] elect_port_base` | UDP port the local elect sidecar binds. |
| `[cluster] hb_interval_ms` | HB emit cadence (default `500`). |
| `[cluster] down_after_ms` | Time without HB before a peer is DOWN (default `5000`). |

---

# Trade-offs and limits

- **Single-node cluster mode is a single process.** It buys client-side key routing, not host-level fault tolerance. Add replicas for that.
- **The proxy port stays available.** It will keep working for non-cluster clients and remains correct, just with the cross-shard hop.
- **Topology is static.** `peers` and `scopes` are read from config at startup. A change is "push new config, restart". There is no gossip, by design.
- **`MOVE-SCOPE` quiesces writes for the prefix.** The window is bounded by slice-ship time, which is single-digit seconds for GB-class scopes over LAN. For prefixes much larger than that, schedule during a maintenance window.
- **Embed as scoped writer is sized for service-shape workloads** (a billing service, an auth service), not multi-TB datasets.
- **Manual rejoin recovery after fallback acceptance.** Copy the fallback's data dir into the writer's before re-enabling; no automatic consensus catch-up.

---

# Out of scope by design

- AUTH and TLS — handled by the deployment edge (sidecar, mesh, LB), not by kevy.
- Multi-DC active-active and CRDTs.
- Raft, Paxos, or any consensus log under the keyspace.
- Gossip-based discovery — `peers` is static.
- Online resharding, `MIGRATE`, `ASK` redirection.
- Multi-master with overlapping ownership — every prefix has exactly one writer at a time.

These will not be added. The simplicity is the feature.

---

# FAQ

**Do I need cluster mode to use replication?**
No. Single-node cluster mode and the replication / multi-node layer are independent. A non-cluster primary can have non-cluster replicas; a cluster primary can have cluster replicas. They compose but neither requires the other.

**Can I run a standard cluster-aware client (Lettuce, ioredis, redis-py-cluster) against a kevy in cluster mode?**
Yes. `CLUSTER SLOTS / SHARDS / NODES` advertise a real partition and `-MOVED` fires on a wrong-shard hit, which is the entire surface those libraries depend on. Stick to the per-shard ports (not the main proxy port) so the client's routing is what reaches the shards.

**What happens to a multi-key command across shards in single-node cluster mode?**
It succeeds. kevy executes cross-slot `MGET`, `MSET`, `SUNION`, transactions, and blocking fan-outs server-side rather than returning `-CROSSSLOT`. `{hashtag}` co-location is still useful for atomicity-sensitive cases but is no longer a correctness requirement.

**How do I survive a writer crash without an operator?**
Declare a fallback for the scope (`prefix=writer|fallback`) and run `kevy-elect` on every node. When the writer misses heartbeats past `down_after_ms`, the fallback starts accepting that prefix's writes; clients receive `-MISDIRECTED writer is <fallback>` and follow. When the original writer comes back, run the manual rejoin recovery.

**Why is gossip / Raft permanently out of scope?**
The cost of a consensus log under every write would erase the throughput and tail-latency advantages that make kevy worth choosing. The static-config + quorum-heartbeat design gives you the failover branch without paying for state-machine replication on the hot path. If your workload genuinely needs a consensus-backed key-value store, kevy is the wrong tool.
