# Primary-replica replication + the read/write-split client (v1.18 / v3-cluster Phase 1)

kevy v1.18 ships the v3-cluster **Phase 1 functional core**: a kevy node can
run as a primary that streams every applied mutation to N read replicas, or as
a replica that connects to a primary and mirrors its keyspace. A new client
crate, `kevy-cluster-rw`, splits writes to the primary and round-robins reads
across replicas.

**Anti-scope reminder** (locked in the plan; do **not** ask for these in v1.18
issues):

- multi-master / sharded-multi-master — only one writer per scope.
- cross-DC active-active / CRDTs.
- Raft / strong-log replication.
- online resharding / gossip discovery — peer list is operator-declared.
- AUTH / TLS — permanently out of scope for kevy.
- chain replication (replica-of-replica) — the dispatch-without-emit gate is
  defensive against the misconfig but the wire shape only supports one hop.

Automatic quorum failover (`kevy-elect`) is Phase 1.5, **not** in v1.18.
Manual promote via `REPLICAOF NO ONE` is the v1.18 failover surface.

## Server side

### Primary

```toml
# kevy.toml
[replication]
role = "primary"
listen_port_base = 16004      # shard i binds replication on this + i
replication_buffer_size = 268435456   # 256 MiB ring backlog per shard
reconnect_window_ms = 60000   # keep a slot for a replica's offset this long
```

Shard `i` binds a dedicated replication TCP listener at `listen_port_base + i`
(per Issue Ledger I2 — mirrors the per-shard cluster listener pattern). Each
applied write is encoded as a RESP envelope (`*2\r\n:<offset>\r\n<argv>`) and
pushed into a per-shard bounded ring backlog; the reactor's pump streams those
frames out to every connected replica on each iteration.

The protocol is RESP3-extended ([`crates/kevy-replicate/docs/wire.md`]). The
offset is `i64`-encoded; at 10 M writes/s the i64::MAX cap is ≈ 30 000 years
out.

### Replica

```toml
[replication]
role = "replica"
upstream = "primary.example:16004"    # primary's listen_port_base
```

When kevy starts with `role = "replica"`, the server spawns one **runner
thread** per local shard. Runner `i` opens a blocking TCP connection to
`(upstream_host, upstream_port_base + i)`, sends the handshake
(`REPLICATE FROM <offset> ID <replica_id>`), reads `+ACK <offset>`, then
loops on the wire stream. Each `ReplicaEvent` (live frame, or one of
`SnapshotBegin` / `SnapshotChunk` / `SnapshotEnd`) is forwarded over an MPSC
channel to the matching shard's reactor thread; the shard drains the channel
once per tick and applies via the usual dispatch path inside a
`ReplicatedApplyGuard` scope.

The guard suppresses the local `ReplicationSource::push_mutation` for the
duration of the apply — without it, a replica that also had a downstream
listener installed would re-emit every applied frame and double-count
offsets. v1.18 forbids chain replication; the gate is defensive.

Snapshot ship: if the replica's requested `from_offset` is no longer in the
primary's backlog (TooOld), the primary in-line-serializes the shard's
keyspace via `kevy_persist::write_snapshot_to`, prefixes with
`+SNAPSHOT\r\n`, streams `$<chunk>\r\n` bulks, and ends with
`+SNAPSHOT_END <ack_offset>\r\n`. The replica accumulates chunks, calls
`kevy_persist::load_snapshot_from` into its local `Store`, then continues at
`ack_offset` for live frames with no gap.

## Commands

| command | effect |
|---|---|
| `ROLE` | `master <offset> []` when no upstream is active, `slave <host> <port> connect 0` when running as a replica. Live state from `REPLICAOF` wins over static config. |
| `INFO replication` | role / connected_slaves / master_repl_offset (master) or master_host / master_port / master_link_status (replica). |
| `REPLICAOF host port` (alias `SLAVEOF`) | Stop any in-flight runner fleet, parse + resolve the new upstream, spawn fresh runners. Replies `+OK`. |
| `REPLICAOF NO ONE` | Stop every runner; demote to standalone (the local store is **not** wiped — operator's choice whether to FLUSH before promoting). |
| `CLUSTER NODES` | The answering node's role flag reflects live replication state (`myself,master` or `myself,slave`). |

## Client side — `kevy-cluster-rw::ReadWriteClient`

```rust
use kevy_cluster_rw::ReadWriteClient;

let mut client = ReadWriteClient::connect(
    ("primary.local", 6004),
    &[("replica1.local", 6004), ("replica2.local", 6004)],
)?;

// Auto-routed: SET goes to primary, GET round-robins replicas.
client.request(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?;
let reply = client.request(&[b"GET".to_vec(), b"k".to_vec()])?;

// READCONSISTENT — force the read to primary (fresh-write-followed-by-read).
let reply = client.request_read(
    &[b"GET".to_vec(), b"k".to_vec()],
    /* consistent = */ true,
)?;
```

v1.18 takes the seed list explicitly — there is no automatic CLUSTER NODES
walk for replica discovery. (A follow-up after release can add an
auto-discover overload for cluster-mode deployments where the operator wants
the client to find replicas itself.)

Write/read classification is in [`kevy_cluster_rw::is_write_verb`]. The table
mirrors `kevy::cmd::is_write_verb` server-side; the duplication is on purpose
(this crate is downstream of `kevy-resp-client` only — it never depends on
the server crate).

## Operating recipes

### Add a fresh replica

1. Start the new kevy with `[replication] role = "replica"` and
   `upstream = "primary:16004"`.
2. The runner connects with `from_offset = 0`. The primary's backlog has long
   evicted offset 0 → TooOld → snapshot ship.
3. After the snapshot loads, the runner resumes at `ack_offset` and lives on
   live frames.

### Re-target a running replica

```
REPLICAOF new-primary.example 16004
```

Stops the old runner fleet (sockets are `Shutdown::Both`'d so any in-flight
read unblocks), parses the new upstream, spawns new runners. Replies `+OK`
within milliseconds. The replica's local store is **kept** — frames from the
new primary land on top. If the operator wants a clean replay, follow with
`FLUSHALL` before or after.

### Manual promote (replica → primary)

```
REPLICAOF NO ONE
```

Stops every runner. Effective role flips to `master`. The local store remains
in whatever state the last applied frame left it. To accept downstream
replicas, also update the config (`role = "primary"` + `listen_port_base`)
and restart — v1.18 does **not** install a downstream listener dynamically.

## Automatic failover via `kevy-elect` (v1.19+ / Phase 1.5)

v1.19 adds quorum-based primary failover on top of v1.18's manual
`REPLICAOF`. Detection is by heartbeat (`HB(epoch, node_id, role,
repl_offset)`) every `hb_interval_ms` (default 200 ms); a peer is flagged
DOWN after `down_after_ms` (default 5 s) without a heartbeat; the alive
replica with the highest `repl_offset` (lowest `node_id` on tie) broadcasts
`OFFER(new_epoch, candidate_id, repl_offset)`; on collecting `N/2 + 1`
`ACCEPT`s it promotes itself via the existing `REPLICAOF NO ONE` path and
broadcasts `ANNOUNCE(epoch, new_primary_id, new_primary_addr)`. Peers
receiving `ANNOUNCE` retarget their `kevy-replicate` runner at the new
primary. Full spec: [`crates/kevy-elect/docs/protocol.md`](../crates/kevy-elect/docs/protocol.md).

### Config

```toml
[cluster]
node_id = "primary-east"              # this node's stable id (≤ 32 B ASCII)
elect_port_base = 16104               # control-plane TCP port (shard 0 = base + 0)
peers = "primary-east@10.0.0.1:16104,replica-1@10.0.0.2:16104,replica-2@10.0.0.3:16104"
```

The `peers` string lists EVERY node in the cluster including this one — the
elector filters self by `node_id` at run-time. Empty `peers` ⇒ kevy-elect is
dormant (v1.18-era configs need no edit).

### Quorum and fault tolerance

| N | quorum | tolerates |
|---|---|---|
| 3 | 2 | 1 down |
| 5 | 3 | 2 down |
| 7 | 4 | 3 down |
| **2** | **2** | **0 down — degenerate, intentionally locked** |

**N=2 warning.** Quorum is `N/2 + 1`, so N=2 needs both nodes alive: either
going down means the survivor cannot reach quorum and **stays read-only**
indefinitely (no writes accepted, no promotion). This is intentional — the
alternative (single-node quorum) would risk a split-brain double-write on
partition. The config linter warns at startup when `peers` lists exactly two
entries. **Recommendation: N ≥ 3** for any deployment that needs automatic
failover. N=2 is acceptable only when "either down = locked" is preferable to
"both down = locked" (extremely rare).

### Split-brain protection

Quorum semantics protect against split-brain by construction: a partitioned
minority cannot reach `N/2 + 1` ACCEPTs, so it cannot promote a new primary.
Once a partition heals, the minority side sees a higher epoch from the
majority side and demotes cleanly — at the cost of dropping any writes that
landed on the minority while partitioned. This is the durability story
v3-cluster Phase 1.5 ships: **writes have guaranteed durability only on the
majority side of any partition.** Use `READCONSISTENT` to avoid stale reads;
the write side cannot retroactively repair minority writes.

### Tunables

| param | default | what it does |
|---|---|---|
| `hb_interval_ms` | 200 | period between outbound HBs per peer |
| `down_after_ms` | 5_000 | mark a peer DOWN after this many ms without HB |
| `election_timeout_ms` | 3_000 | candidate waits this long for quorum ACCEPT |
| `election_backoff_ms` | 1_000–5_000 | random jitter on failed-election backoff |

Tune `hb_interval` × `down_after` to your RTT. Defaults assume a single LAN.
A WAN deployment (which is anti-scope for v1.19 — kevy-elect is single-DC
only) would need higher values to avoid spurious elections during transient
WAN blips.

### Backlog tuning

`replication_buffer_size` is the per-shard ring byte budget. Sizing rule of
thumb:

```
backlog_size ≈ peak_writes_per_sec * avg_argv_bytes * reconnect_window_seconds
```

For 200k writes/sec at 40 B average argv and a 60 s window, 480 MiB per shard
keeps every reconnect on the backlog path. Smaller backlogs are fine —
oversized ones fall back to snapshot ship cleanly.

## Known v1.18 simplifications (tracked as follow-ups)

- **Background snapshot serialization** — *landed in v1.18*. The primary
  freezes a COW `SnapshotView` (O(n) shallow clone — ns/entry) and hands
  it to a worker thread that serializes off the reactor; chunks stream
  back via channel. Reactor pause shrinks to the collect alone.
- **Per-replica peer-addr** — *landed in v1.18*. The ROLE master reply
  carries `(ip, port, offset)` per connected replica; `connected_slaves`
  in `INFO replication` is derived from this list.
- **Replication on io_uring** — *landed in v1.18*. The io_uring reactor's
  tick path drives accept / read / write / pump for replicas; the throughput-
  sensitive write side stays io_uring-native via short-writes + the
  existing non-blocking drain. `KEVY_IO_URING=1` + replication runs and
  matches the epoll reactor's perfgate numbers.
- **CLUSTER NODES live-replica list** — a primary doesn't currently track
  the client-side addresses of its connected replicas (the runner's REPLICATE
  handshake carries only an id). Clients use `kevy-cluster-rw` with explicit
  seeds instead.
- **Auth / link encryption** — never (anti-scope).

## Wire format references

- Live frame envelope: [`crates/kevy-replicate/docs/wire.md`].
- Snapshot ship: [`crates/kevy-replicate/docs/snapshot.md`].
- Handshake: `*5\r\n$9\r\nREPLICATE\r\n$4\r\nFROM\r\n$<n>\r\n<offset>\r\n$2\r\nID\r\n$<m>\r\n<replica_id>\r\n` → `+ACK <offset>\r\n`.

## See also

- [`docs/cluster.md`](cluster.md) — multi-shard exposure + the slot-routing
  `ClusterClient`; orthogonal to (but composable with) replication.
- [`docs/persistence.md`](persistence.md) — RDB / AOF; the snapshot path
  reuses kevy-persist for the on-wire ship format.
- `.claude/plans/2026-06-18-v3-cluster-plan.md` — the canonical execution
  plan; row state reflects what's in this release.

[`crates/kevy-replicate/docs/wire.md`]: ../crates/kevy-replicate/docs/wire.md
[`crates/kevy-replicate/docs/snapshot.md`]: ../crates/kevy-replicate/docs/snapshot.md
[`kevy_cluster_rw::is_write_verb`]: ../crates/kevy-cluster-rw/src/lib.rs
