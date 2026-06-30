# Replication

How kevy streams writes from a primary to one or more replicas, how to fail over by hand or by quorum, and how an embedded process can subscribe to the same stream as a read replica.

## When you need this

Reach for replication when one of these is true:

- **Read fan-out.** A single primary takes every write; one or more replicas absorb the read load and round-robin behind the [`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) client.
- **HA failover.** You want the surviving replicas to elect a new primary automatically when the current one goes away. Add [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) for quorum-based promotion; otherwise promote by hand with `REPLICAOF NO ONE`.
- **Embed-as-replica.** An application uses [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) as an in-process keyspace but wants the source of truth to live on a `kevy` server. The embed mirrors the primary in-memory and serves reads with zero network round-trip; writes are rejected locally and must be sent to the primary.

If you only run one `kevy` node, you do not need this doc. If you need cross-DC active-active, gossip discovery, online resharding, Raft, AUTH, or TLS, kevy will never give them to you — pick a different system.

## Core idea

A primary `kevy` opens a dedicated replication listener per shard. Every applied mutation is encoded as a RESP envelope (`*2\r\n:<offset>\r\n<argv>`) with a monotonically increasing 64-bit offset and pushed into a per-shard bounded ring backlog. Each connected replica streams from its last-acked offset; if the requested offset has aged out of the backlog, the primary in-line-ships a snapshot of that shard's keyspace, then resumes live streaming with no gap. A replica may retarget at runtime with `REPLICAOF host port`, and demote itself with `REPLICAOF NO ONE`. Chain replication (replica-of-replica) is not supported on the wire and is rejected defensively in the apply path.

```
                  +-----------------+
   writes ──────► |    primary      |
                  |  shard 0..N-1   |
                  |  port_base + i  |
                  +--------+--------+
                           │ per-shard RESP stream (offset, argv)
            ┌──────────────┼──────────────┐
            ▼              ▼              ▼
       +---------+    +---------+    +---------+
       | replica |    | replica |    | embed   |
       |   A     |    |   B     |    | (in-proc|
       |  reads  |    |  reads  |    |  reader)|
       +---------+    +---------+    +---------+
```

The same replication stream feeds three kinds of subscribers: a full `kevy` server running as a replica, an embedded `kevy-embedded` `Store` opened in replica mode, and (transitively) the quorum elector that watches everyone's `repl_offset` for failover decisions.

## Worked example

The example below brings up one primary, one replica, retargets the replica at runtime, probes role, and attaches an in-process embedded reader to the same primary.

### 1. Primary `kevy.toml`

```toml
[replication]
role             = "primary"
listen_port_base = 16004        # shard i binds replication on listen_port_base + i
replication_buffer_size = 268435456   # 256 MiB ring backlog per shard
reconnect_window_ms     = 60000       # how long to hold a slot for a reconnecting replica
```

Start it:

```sh
kevy --config /etc/kevy/primary.toml --port 6004
```

Shard 0 of the primary now accepts RESP client traffic on `:6004` and replication connections on `:16004`.

### 2. Replica `kevy.toml`

```toml
[replication]
role     = "replica"
upstream = "primary.internal:16004"   # the primary's listen_port_base
```

Start it on a second host:

```sh
kevy --config /etc/kevy/replica.toml --port 6004
```

Each local shard opens a runner thread, connects to `(upstream_host, upstream_port_base + shard_index)`, handshakes with `REPLICATE FROM <offset> ID <replica_id>`, reads `+ACK <offset>`, then streams frames into the shard's apply path inside a guard that suppresses local re-emission.

### 3. Retarget the replica at runtime

```sh
redis-cli -p 6004 REPLICAOF new-primary.internal 16004
# +OK
```

The replica stops its runner fleet (sockets are shut down so blocked reads unblock), parses the new upstream, and spawns new runners. The local store is **not** wiped — frames from the new primary land on top of the existing data. Call `FLUSHALL` first if you want a clean replay.

### 4. Promote a replica by hand

```sh
redis-cli -p 6004 REPLICAOF NO ONE
# +OK
```

All runner threads stop and the effective role flips to `master`. Local data stays exactly where the last applied frame left it. To accept downstream replicas, you must also edit the config (`role = "primary"` + `listen_port_base`) and restart — the runtime `REPLICAOF NO ONE` does not bind a downstream listener.

### 5. Probe the role

```sh
redis-cli -p 6004 ROLE
# 1) "master"
# 2) (integer) 12345678
# 3) 1) 1) "10.0.0.21"
#       2) (integer) 6004
#       3) (integer) 12345670

redis-cli -p 6004 INFO replication
# role:master
# connected_slaves:1
# master_repl_offset:12345678
# slave0:ip=10.0.0.21,port=6004,offset=12345670
```

Live runtime state from `REPLICAOF` always wins over the static config in the reply.

### 6. Embed-as-replica (one-liner)

An application can join the same replication stream in-process via [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded):

```rust
use kevy_embedded::Store;

let store = Store::open_replica("primary.internal:16004")?;
assert!(store.is_replica());

// Local writes are rejected with READONLY.
assert!(store.set(b"local", b"nope").is_err());

// Reads pay zero network round-trip — the keyspace lives in this process.
if let Some(v) = store.get(b"hello")? {
    println!("{:?}", v);
}
```

The embed connects to the same `listen_port_base` shard, applies frames as they arrive, and serves reads directly from its local arena. A runnable copy lives at [`crates/kevy-embedded/examples/replica.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/replica.rs).

## Knobs

Server-side TOML keys under `[replication]`:

| key | default | meaning |
|---|---|---|
| `role` | `"primary"` | `"primary"` opens a replication listener; `"replica"` spawns runners that pull from `upstream`. |
| `listen_port_base` | `16004` (primary) | Shard `i` of the primary binds replication on `listen_port_base + i`. Replicas connect to the same offset. |
| `upstream` | unset | Replica-only. `host:port` of the primary's `listen_port_base`. Each local shard targets `(host, port + shard_index)`. |
| `replication_buffer_size` | `268435456` (256 MiB) | Per-shard ring backlog in bytes. Reconnects within this window stay on the live path; older offsets trigger snapshot ship. |
| `reconnect_window_ms` | `60000` | How long the primary keeps a slot reserved for a disconnected replica's offset before reclaiming it. |

When [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) is configured, the `[cluster]` block adds quorum knobs:

| key | default | meaning |
|---|---|---|
| `node_id` | unset | Stable id of this node (≤ 32 B ASCII). Used as the tie-breaker in elections. |
| `elect_port_base` | unset | Control-plane TCP port for heartbeats and ballots. Shard 0 binds on `elect_port_base + 0`. |
| `peers` | empty | `id@host:port,…` for every node in the cluster including self. Empty means the elector is dormant. |
| `hb_interval_ms` | `200` | Period between outbound heartbeats per peer. |
| `down_after_ms` | `5000` | A peer is flagged DOWN after this many ms without a heartbeat. |
| `election_timeout_ms` | `3000` | A candidate waits this long for quorum `ACCEPT`s. |

Quorum is `N/2 + 1`. N=2 needs both nodes alive (either down locks the survivor read-only); the linter warns and any deployment that needs failover should use N ≥ 3.

## Trade-offs and limits

Replication is **asynchronous**. The primary commits and replies before it knows any replica has applied the frame; replicas trail by the time it takes a frame to ride the wire and drain the per-shard channel into the apply path. There is no `WAIT`-style barrier and no synchronous mode.

| concern | answer |
|---|---|
| Write durability | Acknowledged by the primary as soon as it lands in the local store and the backlog ring. Replicas catch up afterwards. |
| Read consistency | Replicas may lag. Send `request_read(…, consistent = true)` through `kevy-cluster-rw` to force a read at the primary when read-after-write matters. |
| Replica falls behind | If the reconnect needs an offset that has aged out of the ring, the primary in-line-ships a snapshot of that shard and resumes live frames at the snapshot's end offset — no gap, no operator action. |
| Sizing the backlog | `replication_buffer_size ≈ peak_writes_per_sec × avg_argv_bytes × reconnect_window_seconds`. Oversize is harmless; undersize falls back to snapshot ship. |
| What fails over | Writes to the new primary, automatically when `kevy-elect` is configured, by hand otherwise. Existing `kevy-cluster-rw` clients re-route writes once they learn the new primary; in-flight writes during the gap fail loudly. |
| What does not fail over | Cross-DC traffic, gossip-discovered peers, online resharding, AUTH/TLS — kevy does not ship any of these. Single-DC only. |
| Chain replication | Not on the wire. A replica's apply path will not re-emit downstream; a misconfiguration is rejected defensively. |
| Minority writes during partition | Lost. A partitioned minority cannot reach quorum, cannot promote, and when the partition heals it demotes and accepts the majority's history. Use the consistent-read path on the write side to avoid stale reads. |

The wire format (live frame envelope, snapshot ship, handshake) is documented in [`crates/kevy-replicate/docs/wire.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/wire.md) and [`crates/kevy-replicate/docs/snapshot.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/snapshot.md). The elector's protocol is in [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md).

## FAQ

**How do I promote a replica?**
By hand: connect to the replica and run `REPLICAOF NO ONE`. The effective role flips to `master` immediately, the local store is preserved, and writes are accepted. To accept downstream replicas, also update `role` and `listen_port_base` in the TOML and restart. Automatically: configure `kevy-elect` with `node_id`, `elect_port_base`, and a `peers` list on every node; the alive replica with the highest `repl_offset` wins on quorum.

**Can a replica become a primary, and then back to a replica?**
Yes. `REPLICAOF NO ONE` demotes the upstream link without touching data; a subsequent `REPLICAOF host port` re-attaches to a new primary. The local store is kept across both transitions. Call `FLUSHALL` first if you want a clean replay from the new upstream.

**What's the data loss window?**
The interval between "primary acks the client" and "every replica has applied the frame." Replication is asynchronous, so a primary that crashes after acking a write but before any replica has the frame loses that write. Sizing the gap is workload-dependent — on a single-DC LAN it is typically sub-millisecond. There is no synchronous mode; if you need durability across a power-off, pair replication with [`docs/persistence.md`](https://github.com/goliajp/kevy/blob/develop/docs/persistence.md) (AOF + RDB) on the primary.

**Can I read from a replica?**
Yes — that is the main reason to add one. Use [`kevy-cluster-rw::ReadWriteClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) and it will send writes to the primary and round-robin reads across the replica seeds you pass in. When a read must observe the latest write, use the consistent-read path on the same client to force that read through the primary.

**A replica fell too far behind — how do I recover it?**
Do nothing. The primary detects that the replica's requested offset is no longer in the backlog ring, returns `TooOld`, in-line-ships a snapshot of the shard's keyspace via the same RESP wire connection, then resumes live frames at the snapshot's end offset. The replica swaps in the snapshot, applies the live tail, and is caught up. If you would rather rebuild from empty, stop the replica, delete its data directory, and restart — the runner will connect with `from_offset = 0` and snapshot-ship the whole keyspace.

## See also

- [`docs/cluster.md`](https://github.com/goliajp/kevy/blob/develop/docs/cluster.md) — multi-shard exposure and the slot-routing `ClusterClient`; orthogonal to replication and composable with it.
- [`docs/persistence.md`](https://github.com/goliajp/kevy/blob/develop/docs/persistence.md) — RDB and AOF; the snapshot ship path reuses the same on-disk format on the wire.
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) — the read/write-split client.
- [`crates/kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) — quorum failover.
- [`crates/kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) — embed-as-replica `Store::open_replica`.
