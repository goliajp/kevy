# Replication

How kevy streams writes from a primary to one or more replicas, how to fail over by hand or by quorum, and how an embedded process can subscribe to the same stream as a read replica.

## When you need this

Reach for replication when one of these is true:

- **Read fan-out.** A single primary takes every write; one or more replicas absorb the read load and round-robin behind the [`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) client.
- **HA failover.** You want the surviving replicas to elect a new primary automatically when the current one goes away. Add [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) for quorum-based promotion; use the `FAILOVER` verb for a planned zero-loss handover; or promote by hand with `REPLICAOF NO ONE`.
- **Embed-as-replica.** An application uses [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) as an in-process keyspace but wants the source of truth to live on a `kevy` server. The embed mirrors the primary in-memory and serves reads with zero network round-trip; writes are rejected locally and must be sent to the primary.

If you only run one `kevy` node, you do not need this doc. If you need cross-DC active-active, gossip discovery, online resharding, Raft, AUTH, or TLS, kevy will never give them to you — pick a different system.

## Core idea

A primary `kevy` opens a dedicated replication listener per shard. Every applied mutation is encoded as a RESP envelope (`*2\r\n:<offset>\r\n<argv>`) with a monotonically increasing 64-bit offset and pushed into a per-shard bounded ring backlog. Each connected replica streams from its last-acked offset; if the requested offset has aged out of the backlog, the primary in-line-ships a snapshot of that shard's keyspace, then resumes live streaming with no gap. A replica may retarget at runtime with `REPLICAOF host port`, and demote itself with `REPLICAOF NO ONE`. Chain replication (replica-of-replica) is not supported on the wire and is rejected defensively in the apply path.

Since v3.14 the connection is two-way. The replica writes `REPLCONF ACK <offset>` back on the same replication connection as frames are applied, so the primary holds a per-replica **acked** position (not just "sent") — this is what `INFO replication`'s `slave0` lines and the `WAIT` barrier read. In the other direction the primary appends an out-of-band heartbeat `+PING <generation> <next_offset>` at 1 Hz — it occupies no offset space, and gives the replica self-measured lag (`slave_lag_frames`) and link liveness (`master_link_status`). The `generation` field (v3.16) identifies one unbroken offset history; failover bumps it. The pre-v3.16 one-number `+PING <next_offset>` form still decodes.

Since v3.15 the topology is **symmetric**: a node started with `role = "replica"` binds the full replication listener + source too, so a promoted replica serves downstream replicas immediately — no config edit, no restart.

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

All runner threads stop and the effective role flips to `master`. Local data stays exactly where the last applied frame left it. A node started with `role = "replica"` already binds the full replication listener (topology symmetry, v3.15), so it serves downstream replicas the moment it is promoted — no config edit, no restart. Only a node started `standalone` and retargeted at runtime lacks the downstream listener.

For a coordinated, zero-loss handover (quiesce writes, wait for the target to drain, promote, follow), use the `FAILOVER` verb instead — see *Planned failover* below.

### 5. Probe the role

```sh
redis-cli -p 6004 ROLE
# 1) "master"
# 2) (integer) 12345678
# 3) 1) 1) "10.0.0.21"
#       2) (integer) 6004
#       3) (integer) 12345670

redis-cli -p 6004 INFO replication          # on the primary
# role:master
# connected_slaves:1
# slave0:ip=10.0.0.21,port=6004,state=online,offset=12345670,sent=12345678,lag=8
# master_repl_offset:12345678

redis-cli -p 6004 INFO replication          # on the replica
# role:slave
# master_host:primary.internal
# master_port:16004
# master_link_status:up
# master_last_io_seconds_ago:0
# slave_read_only:1
# slave_repl_offset:12345670
# slave_lag_frames:0
```

Both sides report heartbeat/ACK truth (v3.14): on the primary, a `slave0` line's `state` flips `syncing → online` on the replica's first `REPLCONF ACK`, `offset` is its **acked** position, and `lag` is in frames; on the replica, `master_link_status` is `up` while a heartbeat landed within 3 s, and `slave_lag_frames:0` means caught up. Field-by-field semantics are in the *Observability* section of [`docs/availability.md`](https://github.com/goliajp/kevy/blob/develop/docs/availability.md).

Live runtime state from `REPLICAOF` always wins over the static config in the reply — and with an elect quorum configured, the live election role wins over both.

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
| `role` | `"standalone"` | `"standalone"` = subsystem dormant; `"primary"` opens the replication listener; `"replica"` spawns runners that pull from `upstream`. |
| `listen_port_base` | `0` (= client port + 10000) | Shard `i` binds replication on `listen_port_base + i`. Since v3.15 **replicas bind this listener too** (promotion symmetry). |
| `upstream` | unset | Replica-only. `host:port` of the primary's replication port base. Each local shard targets `(host, port + shard_index)`. |
| `replication_buffer_size` | `268435456` (256 MiB) | Per-shard ring backlog in bytes. Reconnects within this window stay on the live path; older offsets trigger snapshot ship. |
| `reconnect_window_ms` | `60000` | How long the primary keeps a slot reserved for a disconnected replica's offset before reclaiming it. |
| `replica_read_only` | `true` | Reject client writes on a replica with `-READONLY`; the replication apply path and admin verbs bypass the gate. |
| `replica_max_staleness_ms` | `0` (off) | Bounded staleness: a replica whose last primary heartbeat is older than the bound refuses reads with `-STALE`. Ladder rung 3 in [`docs/availability.md`](https://github.com/goliajp/kevy/blob/develop/docs/availability.md). |
| `min_replicas_to_write` | `0` (off) | The primary refuses writes with `-NOREPLICAS` when fewer than N replicas are healthy (live connection that has ACKed). Ladder rung 4. |
| `min_replicas_max_lag_ms` | `10000` | Reserved freshness window for `min_replicas_to_write`. |
| `single_source` | `false` | The upstream is ONE stream on one port (an embedded writer) instead of the per-shard fleet — see *Embedded-as-primary* below. |

Because both roles bind the replication range, co-hosting several instances on one machine requires client ports at least `nshards` apart — otherwise their default replication ranges (`client port + 10000 … + 10000 + nshards − 1`) collide.

When [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) is configured, the `[cluster]` block adds quorum knobs:

| key | default | meaning |
|---|---|---|
| `node_id` | unset | Stable id of this node (≤ 32 B ASCII). Used as the tie-breaker in elections. |
| `elect_port_base` | `0` (= client port + 200) | Control-plane TCP port for heartbeats and ballots — one listener per node. |
| `peers` | empty | `id@host:elect_port:client_port,…` for every node in the cluster including self. Empty means the elector is dormant. |

Use the extended three-field peer syntax: election traffic rides the elect port, while retargeting and `-MISDIRECTED` replies use the client port. The legacy `id@host:port` form assumes both are equal, which is almost never what you want.

Election timing is FIXED CONSTANTS, not config keys: heartbeats every
200 ms, a peer is DOWN after 5 s of silence, a candidate waits 3 s
for quorum ACCEPTs. (Earlier revisions of this page listed them as
`[cluster]` keys — the config parser rejects those.)

Quorum is `N/2 + 1`. N=2 needs both nodes alive (either down locks the survivor read-only); the linter warns and any deployment that needs failover should use N ≥ 3.

One consequence to plan for: in an elect quorum, `[replication] role = "primary"` is only an initial *preference*. Write authority comes from winning an election — every quorum member boots read-only and holds writes until it wins (including a cold start, which pays one election round before the first write). This clamp is what prevents a restarted empty primary from erasing the cluster; the full story is the *Election-only write authority* section of [`docs/availability.md`](https://github.com/goliajp/kevy/blob/develop/docs/availability.md).

## Failover

Two paths move the primary role, both built on the stream mechanics above; the operational detail (steps, timings, error contract) lives in [`docs/availability.md`](https://github.com/goliajp/kevy/blob/develop/docs/availability.md).

**Planned: `FAILOVER host port [TIMEOUT ms] | ABORT`** (v3.15). Run on the primary with the target replica's *client* address; it answers `+OK` and hands over on a background thread: quiesce writes (`-QUIESCED`), poll the target's `INFO replication` until drained (`slave_lag_frames:0`), promote it (`REPLICAOF NO ONE`), then follow it as a replica. The handover retargets to `client port + 10000`, so the target must run with the default `listen_port_base`. Timeout (default 10 000 ms) rolls the quiesce back.

**Crash: quorum election** (v3.15). With the `[cluster]` block on every node, peers detect a dead primary and elect the replica with the highest applied replication offset (lowest `node_id` breaks ties); the winner opens writes and bumps its feed generation, losers retarget automatically. A rejoining ex-primary whose stream is *ahead* of the new primary (a forked suffix of never-replicated writes) gets a **replacing** snapshot resync — `FLUSHALL` before load — instead of a corrupt-close: the fork is discarded and the node converges on the majority's history.

## Trade-offs and limits

Replication is **asynchronous by default**. The primary commits and replies before it knows any replica has applied the frame; replicas trail by the time it takes a frame to ride the wire and drain the per-shard channel into the apply path. When a given write or read needs more, buy it per call: `WAIT n timeout` blocks until ≥ n replicas have acknowledged, `REPL.TOKEN` + `REPL.WAIT` gives read-your-writes on a chosen replica, and two config keys add bounded staleness (`-STALE`) and a min-replicas write gate (`-NOREPLICAS`). The whole ladder, rung by rung, is [`docs/availability.md`](https://github.com/goliajp/kevy/blob/develop/docs/availability.md).

| concern | answer |
|---|---|
| Write durability | Acknowledged by the primary as soon as it lands in the local store and the backlog ring. Replicas catch up afterwards; `WAIT n timeout` blocks until ≥ n have acknowledged (a replica ack is not an fsync — see availability.md). |
| Read consistency | Replicas may lag. Send `request_read(…, consistent = true)` through `kevy-cluster-rw` to force a read at the primary, or use `REPL.TOKEN` + `REPL.WAIT` for read-your-writes on the replica itself. |
| Replica falls behind | If the reconnect needs an offset that has aged out of the ring, the primary in-line-ships a snapshot of that shard and resumes live frames at the snapshot's end offset — no gap, no operator action. |
| Sizing the backlog | `replication_buffer_size ≈ peak_writes_per_sec × avg_argv_bytes × reconnect_window_seconds`. Oversize is harmless; undersize falls back to snapshot ship. |
| What fails over | Writes to the new primary, automatically when `kevy-elect` is configured, by hand otherwise. Existing `kevy-cluster-rw` clients re-route writes once they learn the new primary; in-flight writes during the gap fail loudly. |
| What does not fail over | Cross-DC traffic, gossip-discovered peers, online resharding, AUTH/TLS — kevy does not ship any of these. Single-DC only. |
| Chain replication | Not on the wire. A replica's apply path will not re-emit downstream; a misconfiguration is rejected defensively. |
| Minority writes during partition | Bounded, then lost. A quorum primary that cannot see a strict majority fences its own writes (`-NOREPLICAS primary lost quorum; writes fenced`) within one lease window, so the silent-absorption window is ~5 s, and every write inside it fails loudly. A partitioned minority cannot promote; when the partition heals it demotes, its un-replicated forked suffix is discarded, and it resyncs to the majority's history via snapshot. |

The wire format (live frame envelope, snapshot ship, handshake) is documented in [`crates/kevy-replicate/docs/wire.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/wire.md) and [`crates/kevy-replicate/docs/snapshot.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/snapshot.md). The elector's protocol is in [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md).

## FAQ

**How do I promote a replica?**
Planned and zero-loss: run `FAILOVER host port` on the current primary (see *Failover* above). By hand: connect to the replica and run `REPLICAOF NO ONE` — the effective role flips to `master` immediately, the local store is preserved, writes are accepted, and (since v3.15) the already-bound replication listener starts serving downstream replicas at once. Automatically: configure `[cluster]` with `node_id`, `elect_port_base`, and a `peers` list on every node; the alive replica with the highest applied offset wins on quorum.

**Can a replica become a primary, and then back to a replica?**
Yes. `REPLICAOF NO ONE` demotes the upstream link without touching data; a subsequent `REPLICAOF host port` re-attaches to a new primary. The local store is kept across both transitions. Call `FLUSHALL` first if you want a clean replay from the new upstream.

**What's the data loss window?**
The interval between "primary acks the client" and "every replica has applied the frame." Replication is asynchronous by default, so a primary that crashes after acking a write but before any replica has the frame loses that write. Sizing the gap is workload-dependent — on a single-DC LAN it is typically sub-millisecond. For writes that must survive a single-node loss, follow them with `WAIT 1 <timeout>`: a `WAIT`-acked write exists on two nodes, and crash elections pick the most-advanced replica, so it survives (see [`docs/availability.md`](https://github.com/goliajp/kevy/blob/develop/docs/availability.md)). A replica ack is still not an fsync; for durability across a power-off, pair replication with [`docs/persistence.md`](https://github.com/goliajp/kevy/blob/develop/docs/persistence.md) (AOF + RDB) on the primary.

**Can I read from a replica?**
Yes — that is the main reason to add one. Use [`kevy-cluster-rw::ReadWriteClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) and it will send writes to the primary and round-robin reads across the replica seeds you pass in. When a read must observe the latest write, use the consistent-read path on the same client to force that read through the primary.

**A replica fell too far behind — how do I recover it?**
Do nothing. The primary detects that the replica's requested offset is no longer in the backlog ring, returns `TooOld`, in-line-ships a snapshot of the shard's keyspace via the same RESP wire connection, then resumes live frames at the snapshot's end offset. The replica swaps in the snapshot, applies the live tail, and is caught up. If you would rather rebuild from empty, stop the replica, delete its data directory, and restart — the runner will connect with `from_offset = 0` and snapshot-ship the whole keyspace.

## See also

- [`docs/availability.md`](https://github.com/goliajp/kevy/blob/develop/docs/availability.md) — the operational half: topologies, the consistency ladder, planned + crash failover, the error contract. This page is the mechanics (wire, frames, snapshot ship); that one is what to run and what clients see.
- [`docs/cluster.md`](https://github.com/goliajp/kevy/blob/develop/docs/cluster.md) — multi-shard exposure and the slot-routing `ClusterClient`; orthogonal to replication and composable with it.
- [`docs/persistence.md`](https://github.com/goliajp/kevy/blob/develop/docs/persistence.md) — RDB and AOF; the snapshot ship path reuses the same on-disk format on the wire.
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) — the read/write-split client.
- [`crates/kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) — quorum failover.
- [`crates/kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) — embed-as-replica `Store::open_replica`.

## Embedded-as-primary (v3.2)

An embedded application can be the PRIMARY, with a kevy server as its
replica — read scaling and a full query surface (the replica declares
its own indexes/views/aggregates over the replicated data) for an
in-process store:

```rust
// the application (primary)
let store = Store::open(
    Config::default().with_shards(4).with_embed_writer("127.0.0.1:7101"),
)?;
```

```toml
# the server replica (replica.toml)
[replication]
role = "replica"
upstream = "127.0.0.1:7101"
single_source = true          # ONE upstream stream, hash-routed locally
```

`single_source = true` tells the server the upstream is a single
stream (an embed writer source) rather than the per-shard port fleet
of server↔server replication: one runner connects, keyed frames route
to local shards by key hash, FLUSHALL/FLUSHDB broadcast, and a
snapshot ship broadcasts the whole payload with each shard loading
only its own hash slice.

A replica handshaking from offset 0 (fresh) or from past the backlog
window receives a full snapshot ship from the embed source (the
v1.21 anti-scope, closed in v3.2): a point-in-time freeze of every
shard plus the as-of offset, then live frames.

Relationship to the CDC feed (docs/cdc.md): they coexist by design.
The replication source serves REPLICA CONSISTENCY (infra plane,
per-source offsets); the feed serves APPLICATION CDC ((generation,
offset) cursors, prefix filters, at-least-once). Unifying them would
tie app-facing CDC semantics to the replica protocol.

Gate: `bench/repligate.sh` — true two-process: snapshot ship to a
fresh replica, quiesced digest stability, restart re-sync, and
replica-local `IDX.CREATE`/`IDX.QUERY` over replicated data.

