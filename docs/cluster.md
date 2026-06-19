# Cluster mode + the cluster-aware client (kevy-client 1.9.0)

kevy is a **single-node, multi-shard** engine. Cluster mode is not multi-host
distribution (there is no failover, gossip, online resharding, or
MIGRATE/ASK — those are permanently out of scope). It is a way to expose each
internal shard as an addressable cluster node so a key-aware client talks
**straight to the shard that owns the key**, skipping the server-side
cross-shard forwarding hop.

That hop is the whole point: under the default single-port proxy behaviour, a
command landing on the wrong shard is forwarded internally to the owner. That
forward is correct but it costs — it dominates tail latency at low load and
throughput at high load (measured: see [Performance](#performance) below).
Cluster mode + a routing client removes it.

## Server side — `--cluster`

```sh
kevy --threads 8 --cluster          # main port 6004, shard ports 6005–6012
```

`--cluster` (or `KEVY_CLUSTER=1`, or `[cluster] enabled = true`) does three
things:

- **Per-shard listeners.** Shard `i` gets a deterministic extra port at
  `port + 1 + i` (override the base with `[cluster] port_base`). The main port
  keeps full proxy-style behaviour for everything else.
- **Real topology reporting.** `CLUSTER SLOTS / SHARDS / NODES` advertise the
  actual partition: CRC16 `{hashtag}` slots, one contiguous range per shard.
  `CLUSTER KEYSLOT` / `COUNTKEYSINSLOT` / `MYID` are implemented and agree with
  upstream Redis.
- **`-MOVED` instead of forwarding.** A wrong-shard key arriving on a cluster
  port answers `-MOVED <slot> <host:port>` rather than being proxied. Correct
  routing means `-MOVED` never fires.

Switching an existing data dir in or out of cluster mode re-homes keys once at
startup; the source files are backed up as `*.premigration.<ts>`.

Stock cluster-aware tools — `redis-cli -c`, `redis-benchmark --cluster`, and
mainstream client libraries — work directly against the cluster ports because
the protocol subset is faithful.

## Client side — `ClusterClient`

`kevy-client` 1.9.0 ships a typed routing client so you don't need a full
third-party cluster library:

```toml
[dependencies]
kevy-client = "1.9.0"
```

```rust
use kevy_client::ClusterClient;

// Connect to any cluster port as a seed; the topology is discovered via
// CLUSTER SLOTS and one connection is opened per shard.
let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;

cc.set(b"user:42", b"alice")?;
let v = cc.get(b"user:42")?;            // routed to user:42's owner shard
let n = cc.incr(b"counter")?;

// Multi-key DEL/EXISTS may span shards — routed per key and summed.
let removed = cc.del(&[b"a", b"b", b"c"])?;
# Ok::<(), std::io::Error>(())
```

A runnable version is in
[`crates/kevy-client/examples/cluster.rs`](../crates/kevy-client/examples/cluster.rs):

```sh
kevy --port 6004 --threads 4 --cluster          # shards at 6005–6008
cargo run -p kevy-client --example cluster -- 6005
```

### How routing works

1. **Discover.** `connect` sends `CLUSTER SLOTS` to the seed, which returns each
   shard's `[start, end, host, port]`. The client builds a `slot → shard-index`
   table (16384 entries) and opens one `RespClient` per distinct shard node.
   Because the table comes from the server's *actual* advertised ranges, the
   client never has to replicate the server's `slot → shard` arithmetic.
2. **Route.** Every single-key command computes `key_hash_slot(key)` (CRC16
   XMODEM over the `{hashtag}` if present, else the whole key) and sends the
   request to that slot's owner connection. No `-MOVED`, no forwarding.
3. **Fan-out where needed.** `DBSIZE` / `FLUSHALL` are whole-cluster — kevy fans
   these out server-side (`Route::Dbsize` / `Route::Flush`), so one call already
   reports/wipes the entire cluster; the client does not sum them itself.

### Command coverage

| Group | Commands |
|-------|----------|
| String | `set`, `set_with_ttl`, `get`, `incr`, `incr_by`, `expire`, `persist`, `ttl_ms` |
| Keys (multi, per-key routed) | `del`, `exists` |
| Whole-cluster (server fan-out) | `dbsize`, `flushall` |
| Keyless | `ping`, `publish` |
| Hash | `hset`, `hget`, `hdel`, `hlen`, `hgetall`, `hkeys`, `hvals` |
| List | `lpush`, `rpush`, `lpop`, `rpop`, `llen`, `lrange` |
| Set | `sadd`, `srem`, `smembers`, `scard`, `sismember`, `sinter`, `sunion`, `sdiff` |
| Sorted set | `zadd`, `zrem`, `zscore`, `zcard`, `zrange` |

For anything not wrapped, drop to the raw routed helpers:

```rust
// Route an arbitrary single-key command to its owner shard.
let reply = cc.request_keyed(b"mykey", &[b"STRLEN".to_vec(), b"mykey".to_vec()])?;
// A keyless command answered by any shard.
let reply = cc.request_unkeyed(&[b"PING".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

### Multi-key same-slot limit

The set-combine operations (`sinter` / `sunion` / `sdiff`) route by their
**first** key. Like Redis Cluster, all of their keys must live in the same
slot — use a shared `{hashtag}` so they hash together:

```rust
cc.sadd(b"{users}:active",  &[b"a", b"b"])?;
cc.sadd(b"{users}:premium", &[b"b", b"c"])?;
let both = cc.sinter(&[b"{users}:active", b"{users}:premium"])?; // same slot → OK
# Ok::<(), std::io::Error>(())
```

Without a shared hashtag the keys land on different shards and the server
answers `-MOVED` (surfaced as an `io::Error`). `del` / `exists` are **not** so
constrained — they route each key independently and sum the results.

Pub/sub does **not** need a cluster-aware subscriber: kevy's pub/sub is
process-global (a message published to any shard is delivered to subscribers on
every core), so a normal `Subscriber` connected to any single port sees all
messages. `ClusterClient::publish` likewise just sends to one shard.

## Performance

Measured on a clean lx64 16-core bare-metal box, server and client on disjoint
cores, GET workload at concurrency 64:

| client path | throughput | p99 latency |
|-------------|-----------:|------------:|
| single-shard proxy (cross-shard hop) | 333 k ops/s | 3858 µs |
| **`ClusterClient` (zero hop)** | **533 k ops/s** | **260 µs** |

That's **1.6× the throughput and ~15× lower tail latency** — purely from
removing the forwarding hop. The typed `ClusterClient` hits the same ceiling as
a hand-rolled raw-socket router, so the typed API adds no measurable overhead.
Reproduce with `cargo run -p kevy-client --release --example cluster_bench`.

> Run the perf bench on a clean, core-isolated machine. On a small co-located
> cloud VM the cross-shard hop's cost is buried in scheduling noise — that
> nearly misled the investigation into concluding the hop didn't matter.

## When to use it

- **Use `ClusterClient`** when a single client drives enough load that the
  forwarding hop shows up — high-throughput or tail-latency-sensitive workloads.
  It is the recommended path for self-hosting kevy under load.
- **Stick with the plain `Connection` / single port** for ordinary use: the
  proxy behaviour is correct and simpler, and at low load the hop is cheap.
- **Reach for `redis-cli -c` / a third-party cluster client** only for
  interop testing; the native `ClusterClient` is lighter for Rust callers.

## Read-write split: combining cluster mode with replication (v1.18)

`kevy-cluster-rw` is a sibling client for the **replication** topology — a
primary kevy node serving writes + a fleet of replica kevy nodes serving
reads (see [`docs/replication.md`](replication.md) for the server side). It
is **orthogonal to cluster mode**: the replication topology is one writer per
*process*, while cluster mode partitions one process into N shards. They
compose — a primary in cluster mode advertises N shards, every replica also
runs N shards, and the operator wires up `kevy-cluster-rw` between them.

```rust
use kevy_cluster_rw::ReadWriteClient;

let mut client = ReadWriteClient::connect(
    ("primary.local", 6004),
    &[("replica1.local", 6004), ("replica2.local", 6004)],
)?;
// Writes → primary, reads round-robin across replicas (fallback to primary
// when the fleet is empty). `consistent = true` forces a read to primary.
client.request(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?;
let reply = client.request(&[b"GET".to_vec(), b"k".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

The crate is a v1.18 release add. Per-command read/write classification
lives in `kevy_cluster_rw::is_write_verb`. v1.18 takes the seed list
explicitly (no automatic `CLUSTER NODES` discovery — a follow-up after
release); the operator's deployment scripts list primary + replica
addresses.

## embed-as-read-replica (v1.20 / Phase 2)

A `kevy-embedded` store can subscribe to a server primary's replication
stream and mirror its keyspace in-process. Reads pay zero network
round-trip; writes are refused locally and must go to the primary.

```rust
use kevy_embedded::Store;

// One-liner: in-memory replica, AOF off, default reconnect (100 ms → 5 s).
let replica = Store::open_replica("primary.local:16004")?;

// Reads work; writes return io::Error("READONLY ...").
let v = replica.get(b"hello")?;
assert!(replica.set(b"k", b"v").is_err());
# Ok::<(), std::io::Error>(())
```

For full control:

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

The handshake sends `REPLICATE FROM <last-applied-offset> ID <replica_id>`
to the primary's replication listener (default `port + 10000` on the
server side, configured via the server's `--replication-listener` /
`[replication] listen_port`). The primary acks the offset and starts
streaming frames; the embed runner thread applies each frame onto the
local shards via the same dispatch path the server-side replica uses,
then advances the local offset for resume on reconnect.

### v1.20 scope (MVP)

- **Single upstream URL = single primary shard mirror.** Multi-shard
  upstream is "spawn one `Store::open_replica` per primary shard"
  for now; a runner-per-URL convenience surface lands in a follow-up.
- **No local AOF on a replica.** `open_replica` force-disables it (a
  local AOF would diverge across restarts and double-apply on the
  next open). For durability across replica restarts, hold the
  upstream's backlog long enough that the replica's last-applied
  offset is still on disk.
- **No snapshot ingest.** A replica connecting at offset 0 against a
  primary whose backlog has rolled past that point currently drops
  the connection; full snapshot ingest (`+SNAPSHOT ... +SNAPSHOT_END`)
  is a v1.20.x follow-up.
- **No auto-retarget on `kevy-elect` ANNOUNCE.** Manual reconfigure
  for primary changes until the failover hook lands; pair with
  `kevy-cluster-rw`'s topology refresh for a fully-automated path.
- **PUBLISH is allowed locally on a replica.** Pub/sub is process-
  local in kevy (not replicated), so a local PUBLISH only reaches
  this process's subscribers; the keyspace itself remains read-only.

### Failure modes

- **Primary down** — runner reconnects with exponential backoff
  (`Config::with_replica_reconnect`, default 100 ms → 5 s). Reads
  keep working against the last-applied snapshot; writes still
  return `READONLY`.
- **Offset gap** — the wire client surfaces `OffsetGap`; the runner
  drops the connection so the next reconnect picks up from the new
  applied offset (which now lags the primary). v1.20.x snapshot
  ingest closes this gap automatically; v1.20 requires the operator
  to refresh from a snapshot manually.
- **Replica drop** — the runner thread is joined on the last
  `Store` clone drop; the primary's listener observes a clean FIN
  and frees the per-replica slot.

## Scoped multi-writer (v1.21 / Phase 3)

`kevy-scope` lets the operator declare per-prefix ownership: a
specific writer node owns writes for keys matching `<prefix>`;
every other node answers `-MISDIRECTED writer is <host:port>`
so the client follows. An optional fallback takes over when
`kevy-elect` flags the writer DOWN.

```toml
[cluster]
node_id = "embed-billing-1"
elect_port_base = 16100
peers   = "embed-billing-1@10.0.0.1:6004,server-eu-1@10.0.0.2:6004,reader-1@10.0.0.3:6004"
# prefix=writer[|fallback], comma-separated. Embedded `:` in the
# prefix is fine (the first `=` splits prefix from owner spec).
scopes  = "app:billing:=embed-billing-1|server-eu-1, app:auth:=embed-auth-1"
```

### Anti-scope (locked in the v3-cluster RFC)

- **No Raft, no gossip.** The ownership table is static config;
  the elect quorum signals only "writer DOWN → fallback takes
  over", not topology consensus.
- **No write-shadowing during migration.** `MOVE-SCOPE` runs as
  Q3=(a) quiesce-window: the writer pauses writes for the prefix,
  ships its slice, then ownership flips. Operator-coordinated,
  no double-acceptance window.
- **No automatic migration.** `MOVE-SCOPE` is operator-issued;
  the cluster never decides to move a scope on its own.

### Wire shapes

- `-MISDIRECTED writer is <host:port>` — the write landed on a
  node that is not this scope's writer (or active fallback).
  `kevy-cluster-rw` 1.21+ caches the per-key target and retries
  transparently; v1.20-and-earlier clients propagate the error.
- `-QUIESCED migrating to <host:port>` — transient during a
  MOVE-SCOPE window. Clients should back off briefly and retry
  rather than panic; the quiesce window is bounded by the slice
  ship time (single-digit seconds for GB-class scopes over LAN).

### Embed as writer

A scope's writer can be an embed (`embed-as-writer`) or a server.
For embed:

```rust
use kevy_embedded::{Config, Store};

let writer = Store::open(
    Config::default().with_embed_writer("0.0.0.0:6105")
)?;
// Every local write pushes into the embed's replication source
// backlog; readers connect to `0.0.0.0:6105` via
// `kevy_replicate::ReplicaClient`.
writer.set(b"app:billing:invoice:42", b"...")?;
# Ok::<(), std::io::Error>(())
```

### F4 fallback

When `kevy-elect` reports the scope's writer in `down_peers`
(last HB older than `down_after`, default 5 s), the declared
fallback treats itself as the active owner. Writes on the
fallback succeed; writes on every other node continue to
MISDIRECT, now naming the fallback. When the writer's HBs
resume, the auto-reclaim is implicit — the writer leaves
`down_peers`, so the fallback steps down.

**Manual rejoin recovery (v1.21).** If the writer was DOWN long
enough that the fallback accepted writes, those writes only
exist on the fallback. Before re-enabling the writer for the
scope: stop the writer, copy the fallback's data dir into the
writer's, then restart the writer. v3.1 automates this via a
writer-replica handshake from the fallback's stream;
v1.21 keeps it manual to stay inside the "no fancy consensus"
contract.

### MOVE-SCOPE

```
MOVE-SCOPE <prefix> from <from-node-id> to <to-node-id>
```

Issued against the source writer. Walks the Q3=(a) quiesce
window:

1. The writer flips its local migration state to MIGRATING for
   `<prefix>`; subsequent writes to keys under the prefix
   answer `-QUIESCED migrating to <to-host:port>`.
2. The writer serializes the prefix's keyspace slice and ships
   it via `MOVE-SCOPE-INGEST <prefix> <bulk>` to the target's
   data port.
3. On `+OK`, the writer commits the migration locally: future
   writes for the prefix on the source now return
   `-MISDIRECTED writer is <to-host:port>` (no quiesce — the
   move is done).
4. Other cluster members continue routing per their static
   `[cluster] scopes` config until the operator pushes new
   config + restarts (v1.21 has no gossip).

**Limitations (v1.21):**
- The migration state is per-node local. Other members of the
  cluster need a config push + restart to learn the new writer.
- The data ship serializes the entire prefix slice in memory.
  For prefixes ≫ GB, schedule MOVE-SCOPE during a maintenance
  window; embed-as-writer's MVP isn't sized for that scale.
- Aborting mid-ship reverts to the source writer; no partial-
  apply state is left on the target.
