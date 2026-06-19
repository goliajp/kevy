# Changelog

All notable changes to kevy. The format is loosely
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); kevy's release
cadence is "tag when a Wave closes," not strict semver below v1.0.

## [v1.22.0] — 2026-06-20 (v3-cluster close — Phase 2 + Phase 3 + Phase 4)

Bundle release closing v3-cluster: **embed-as-read-replica**
(Phase 2), **scoped multi-writer** (Phase 3), and **async client**
(Phase 4). Three phases shipped together as one coherent v3
upgrade per user policy. Server / persistence / pub-sub paths are
unchanged from v1.19; this release lands new surface across
`kevy-embedded`, `kevy-client`, the new `kevy-scope` and
`kevy-client-async` crates, plus the cluster cement in `kevy/`
and topology refinements in `kevy-cluster-rw` / `kevy-elect`.

---

### Phase 2 — embed-as-read-replica

An application embedding `kevy-embedded` can mirror a server
primary's keyspace in-process — reads pay zero network round-trip;
local writes return `READONLY` (the replication stream is the only
writer). Same `kevy_replicate::ReplicaClient` wire client that
drives v1.18 server replicas drives the embed runner.

- **`kevy_embedded::Store::open_replica(upstream)`** — convenience
  constructor (`without_aof` + upstream + default reconnect
  100 ms → 5 s). Returns a normal `Store` with
  `is_replica() == true`; cloneable and droppable like any other.
- **`Config::with_replica_upstream(host:port)`,
  `with_replica_id(id)`, `with_replica_reconnect(min, max)`** —
  full builder control. Default replica id is
  `kevy-embedded-replica`; override per process when multiple
  replicas share one primary.
- **`Store::is_replica()`** — live query of replica mode.
- **READONLY enforcement** — every mutating embed API
  (`set` / `del` / `incr_by` / `expire` / `flushall` / `hset` /
  `hdel` / `lpush` / `rpush` / `lpop` / `rpop` / `sadd` / `srem` /
  `zadd` / `zrem` / `persist`) returns
  `io::Error::other("READONLY ...")` on a replica. Wire string
  mirrors the server-side `-READONLY` reply so applications
  pattern-match the same way against both backends. `PUBLISH`
  remains allowed (pub/sub is process-local).
- **`kevy_embedded::replica_runner` (pub(crate))** — one
  background thread per `Store::open_replica`, drives a real
  `kevy_replicate::ReplicaClient`. Exponential reconnect
  (sliceable so shutdown is acted on within `backoff_min`),
  interruptible `next_event`, joined on last `Store` clone drop
  via `DropGuard`.
- **`docs/cluster.md` "embed-as-read-replica" section** + runnable
  example `crates/kevy-embedded/examples/replica.rs`.

Internals: new `replica_glue.rs` (`spawn_replica_runner`,
`ensure_writable`), extracted `store_persist.rs` to keep
`store.rs` under the 500-LOC project ceiling.

Anti-scope contracts: single upstream URL = single primary shard
mirror (multi-shard upstream is "spawn N replicas" for v1.22). No
snapshot ingest (a replica connecting at offset 0 against a
primary whose backlog has rolled past drops the connection — full
ingest is a follow-up). No auto-retarget on `kevy-elect`
ANNOUNCE; pair with `kevy-cluster-rw` topology refresh for the
automated path. No replica writes — `READONLY` is the contract.

---

### Phase 3 — scoped multi-writer

Per-prefix writer ownership with optional server-backed fallback,
longest-prefix routing, `-MISDIRECTED writer is <host:port>`
redirect, and `MOVE-SCOPE` quiesce-window migration
(Q3 = (a) per RFC). Embed-as-writer joins the cluster as a source:
writes pushed into a replication-source backlog, served to
subscribers (server replicas + embed read-replicas) over the same
wire protocol Phase 2 introduced.

- **new `kevy-scope` crate** — pure-data stone layer:
  `Scope` / `OwnershipTable` (longest-prefix routing + overlap
  linter + F4 fallback) / `MigrationTable`
  (start/commit/abort/lookup).
- **`kevy-config`** — `[cluster] scopes = "prefix=writer[|fallback],..."`
  flat-string parser (same shape rationale as v1.19's `peers`).
- **`kevy/src/scope_integration.rs`** — process-global ownership
  + peer-addr resolution + migration state + ingest guard +
  wire encoders.
- **`kevy/src/ops/scope_move.rs`** — `MOVE-SCOPE` +
  `MOVE-SCOPE-INGEST` cement (operator-issued; serialize prefix
  slice → ship via RESP2 → ingest with route bypass → commit/abort).
- **`kevy-cluster-rw::ReadWriteClient`** — follows `-MISDIRECTED`
  (per-key target cache, lazy conn cache) + retries on
  `-QUIESCED` (exponential backoff 5 ms → 80 ms, 7 attempts).
- **`kevy-embedded::replica_source`** — embed-as-writer TCP
  listener + accept loop + per-conn streaming threads. Reuses
  `kevy_replicate::source::ReplicationSource`.
- **`kevy-elect::ElectorSnapshot.down_peers`** — exposes per-peer
  liveness for F4 fallback decisions.

Wire shapes (Q3 = quiesce-window MOVE-SCOPE):
- `-MISDIRECTED writer is <host:port>` — final redirect
  post-migration commit.
- `-QUIESCED migrating to <host:port>` — transient during quiesce
  window; client backs off + retries against original primary;
  once committed, primary returns `-MISDIRECTED` and client
  follows.

Server-side bug fix: `dispatch.rs` GET/SET fast path was BELOW
the scope routing check; SET silently bypassed scope ownership.
Moved scope routing ABOVE the fast path (one Relaxed atomic load
per dispatch, below measurable noise per perfgate).

Anti-scope (locked): No Raft / gossip / online resharding /
MIGRATE-ASK. No write-shadowing during migration. No automatic
migration (operator-issued only). No cross-scope transactions.
Auto writer-reclaim deferred to v3.1 (v1.22 ships the manual
recovery procedure in `docs/cluster.md`).

Docs + example: `docs/cluster.md` "Scoped multi-writer" section;
`crates/kevy-embedded/examples/scoped_writer.rs` demonstrates the
embed-as-writer pattern.

---

### Phase 4 — `kevy-client-async`

Apps already on tokio / smol / async-std get a 1:1 async surface
with the blocking client plus pipeline-first batch sugar
(RFC Q4 part b) that collapses N sequential round-trips into one.
The blocking `kevy-client` stays the default and remains 0-dep;
async is opt-in.

- **new `kevy-client-async` crate** (v1.0.0, sole dep-rule
  exemption — RFC F5). 3 feature-gated transports:
  - `tokio` — `tokio::net::TcpStream`, default-features = false,
    minimum surface `["net", "rt", "io-util"]`.
  - `smol` — `smol::net::TcpStream`, default-features = false.
  - `async-std` — `async_std::net::TcpStream`. Each dep line
    carries an inline `# EXEMPTION — see
    feedback-pure-rust-no-c-principle.md` comment per the
    project's audit rule. T4.8 enforces exactly-one-runtime at
    compile time (`compile_error!` on zero or more than one).
    `default = ["tokio"]` as a dev convenience; lib consumers
    should set `default-features = false`.
- **Runtime-agnostic core.** Self-defined `AsyncRead` /
  `AsyncWrite` / `AsyncTransport` traits in the futures-io shape
  (`&mut [u8]`, `Poll<io::Result<usize>>`). Each runtime ships a
  thin per-type adapter that implements our traits on top of its
  `TcpStream`. No binding to `futures-io` / `tokio::io::AsyncRead`
  — that would bleed an ecosystem dep into the core.
- **`AsyncRespCodec<T>`** — async equivalent of
  `kevy_resp_client::RespClient`. Same state machine; reuses
  `kevy_resp::{encode_command, parse_reply}` so wire format has
  one implementation. `request` / `send` / `read_reply` /
  `pipeline` cover per-command and batched paths.
- **`AsyncConnection`** — TCP mirror of `kevy_client::Connection`.
  `open(url).await`, `from_transport(stream)`, plus 42 1:1 async
  methods across string / hash / list / set / sorted-set families.
- **`AsyncSubscriber`** — TCP mirror of
  `kevy_client::Subscriber`. connect / open / subscribe /
  psubscribe / unsubscribe / punsubscribe / recv / recv_message /
  hello3. `set_read_timeout` intentionally not mirrored — async
  timeouts live at the runtime layer.
- **`AsyncClusterClient`** — TCP mirror of
  `kevy_client::ClusterClient`. CLUSTER SLOTS topology discovery,
  one AsyncRespCodec per shard, CRC16 routing. 14 mirror methods.
- **Pipeline-first sugar.** `AsyncConnection::pipeline()` returns
  a typed-by-name builder (15 commands + `push_raw` escape).
  `run(&mut conn).await -> io::Result<Vec<Reply>>` — single TCP
  round-trip. Per-command errors surface as `Reply::Error(_)`
  inside the Vec. `into_cmds()` degrades cleanly onto a blocking
  client.
- **URL parser** — `kevy://` / `redis://` / `tcp://` schemes
  accepted. `mem://` / `file://` rejected with a pointer at the
  blocking client.
- **Examples** — `examples/tokio_hello.rs` +
  `examples/pipeline.rs`.
- **`docs/async.md`** — full guide. README gains an "As an
  async-runtime client" subsection.

---

### Tests + perfgate

- `cargo test --workspace -- --test-threads=4` → **1069 passed,
  0 failed** (was 996 at v1.20 baseline; +73 across P2 / P3 / P4).
- `cargo clippy --workspace --all-targets -- -D warnings` → clean.
  Per-runtime `--features {tokio,smol,async-std} --all-targets --
  -D warnings` clean under all three.
- New e2e: `server_replica_e2e` (P2, 3 tests), `embed_writer_e2e`
  + `scope_misdirected_e2e` + `scope_move_e2e` smoke (P3, 4
  tests), `tokio_basic` + `smol_basic` + `async_std_basic` (P4,
  5+4+4 tests).
- `bench_vs_blocking.rs` — 3 `#[ignore]` benches the operator
  runs against a live kevy server.
- lx64 perfgate PASS 6/6 on P3 commit `5649148` (scope routing
  added to dispatch hot path without measurable regression). P4
  perfgate by-construction (server / blocking client paths
  unchanged).

### Versions

- workspace `1.19.0` → `1.22.0`
- `kevy-embedded` `1.3.0` → `1.4.0` (P2 + P3 surface added)
- `kevy-client` `1.10.0` → `1.11.0` (P2:
  `Connection::Embedded(Box<Store>)` — pattern-matches need
  `Box`-aware adjustment; rebuild required)
- new crate `kevy-client-async` `1.0.0` (sole crates.io dep
  exemption — tokio / smol / async-std feature-gated)
- new crate `kevy-scope` `1.22.0`
- workspace `rust-version` pin removed — track the latest stable
  Rust toolchain (CI builds against current stable).

### Deferred to production-vet / v1.22.x

- T3.17 embed-writer-crash + fallback-takeover integration
  (F4 algorithm unit-tested in `kevy-scope`; multi-process elect
  integration left to actual deploys).
- Multi-shard replica upstream (currently 1 URL = 1 primary shard
  mirror).
- Replica snapshot ingest on offset-zero with rolled backlog.
- Auto writer-reclaim on F4 path (manual recovery shipped here).

## [v1.19.0] — 2026-06-19 (Phase 1.5 — automatic primary failover)

**v3-cluster Phase 1.5 — quorum-based automatic primary failover.**
Detection is by heartbeat every 200 ms; a peer is flagged DOWN after
5 s without a heartbeat; the alive replica with the highest
`repl_offset` (lowest `node_id` on tie) becomes a candidate and
broadcasts `OFFER`; on `N/2 + 1` `ACCEPT`s the candidate promotes
via the existing `REPLICAOF NO ONE` path and broadcasts `ANNOUNCE`.
Peers receiving `ANNOUNCE` retarget their `kevy-replicate` runner
at the new primary.

### Added

- **`kevy-elect` crate** — quorum failover layer on top of the v1.18
  manual `REPLICAOF` primitive. Pure-Rust 0-dep, RESP2 control plane
  over TCP (separate port per shard; election state is per-node).
  Public surface: `Transport::spawn(elector, hb_interval, listen,
  peers)`, `Transport::state_snapshot()`, `Transport::set_repl_offset()`,
  `Transport::shutdown()`.
- **Election state machine** (`Elector` struct): pure-logic core
  with `tick(now) → Vec<Outbound>` and `on_message(from, msg, now)`,
  exhaustively unit-tested against quorum / split-brain / dueling /
  rejoin / N=2 degenerate scenarios via an in-memory multi-elector
  simulator (`Sim`).
- **TCP transport**: one listener thread + one outbound thread per
  peer + one orchestrator thread, all interruptible via short
  read/accept timeouts (no Mutex on the hot path). Real-socket e2e
  test on loopback: 3-node primary kill → replica promotes in ~1 s.
- **`[cluster]` config extension**: `node_id`, `elect_port_base`,
  `peers = "id@host:port,..."` (flat-string shape, no parser
  extension needed). v1.18-era configs need no edit — kevy-elect is
  dormant unless both `node_id` and `peers` are set.
- **`ANNOUNCE` epoch handling**: a rejoining old primary sees a
  higher epoch on its first heartbeat to the new majority and
  demotes cleanly. No double-write — the partitioned minority never
  reached quorum so its writes had no durability guarantee.

### Anti-scope (locked)

Not Raft. No log replication consensus. No gossip discovery (peer
set is operator-declared). No cross-DC (RTT assumptions are LAN-
scale). No online membership change. No TLS / auth on the control
plane (consistent with v1.18 anti-scope).

### Recommendations

- **N ≥ 3** for any deployment that needs automatic failover. N=2 is
  intentionally locked when either node is down (config linter warns
  at startup).
- Tune `hb_interval_ms` × `down_after_ms` to your LAN's RTT; the
  defaults (200 ms / 5 s) assume sub-millisecond network.
- Use `READCONSISTENT` on the read side to avoid stale reads across
  a partition; the write side cannot retroactively repair minority
  writes.

### Documentation

- New "Automatic failover via kevy-elect" section in
  [`docs/replication.md`](docs/replication.md) — config, quorum
  table, split-brain protection, tunables.
- Full wire spec in
  [`crates/kevy-elect/docs/protocol.md`](crates/kevy-elect/docs/protocol.md).

### Tests

- 36 kevy-elect unit / sim tests (algorithm + 6 chaos drills via
  `Sim`).
- 1 real-TCP loopback e2e covering the 3-node primary-kill →
  promote path.

## [v1.18.0] — 2026-06-18

**v3-cluster Phase 1 — primary-replica replication + read/write split client.**
A kevy node can now run as a primary that streams every applied mutation to N
read replicas, or as a replica that connects to a primary and mirrors its
keyspace. Manual failover via `REPLICAOF` / `REPLICAOF NO ONE`. New companion
client `kevy-cluster-rw` splits writes to the primary and round-robins reads
across replicas.

### Added

- **Replication backlog + per-shard listener** (`[replication] role =
  "primary"`). Each applied mutation is encoded as a RESP envelope
  (`*2\r\n:<offset>\r\n<argv>`) and pushed into a per-shard bounded ring
  backlog; the reactor's pump streams frames out to connected replicas on
  each iteration. Per-shard listener binds at `listen_port_base + shard_id`
  (mirrors the cluster-listener pattern; per Issue Ledger I2). Tunable
  backlog size + reconnect-window slot retention.
- **Server-as-replica** (`[replication] role = "replica"` + `upstream =
  "host:port"`). At startup kevy spawns one runner thread per local shard,
  each holding a blocking `ReplicaClient` to the matching upstream shard
  port. Events flow to the shard's reactor over an MPSC channel and apply
  on the reactor thread under a `ReplicatedApplyGuard` (prevents chain-
  replication re-emit).
- **Snapshot ship** for fall-behind replicas. When a replica's `from_offset`
  is no longer in the primary's backlog (TooOld), the primary in-line-
  serializes the shard's keyspace via `kevy_persist::write_snapshot_to`,
  streams as `+SNAPSHOT` / `$<chunk>` / `+SNAPSHOT_END <ack_offset>`, and
  the replica loads via `kevy_persist::load_snapshot_from` then resumes on
  live frames with no gap.
- **`REPLICAOF host port`** / **`REPLICAOF NO ONE`** (alias `SLAVEOF`) — full
  dynamic retarget + demote. Stops in-flight runners (via `try_clone`'d
  socket + `Shutdown::Both` to break the blocking read), parses + resolves
  the new upstream, spawns fresh runner fleet. Effective role flips live;
  `ROLE` / `INFO replication` / `CLUSTER NODES` all report from live state,
  overriding static config.
- **`ROLE`** — Redis-shape reply. Master form: `["master", offset,
  [[ip, port, offset], ...]]` (per-replica array populated via the
  `getpeername(2)` capture added in this release). Slave form:
  `["slave", host, port, "connect", 0]`.
- **`INFO replication`** — full section with `role` / `connected_slaves` /
  `master_repl_offset` (master block) or `master_host` / `master_port` /
  `master_link_status` / `slave_read_only` / `slave_repl_offset` (slave
  block).
- **`kevy-cluster-rw::ReadWriteClient`** — companion client crate. Operator-
  supplied seed list (primary + replicas), one connection per node. Auto-
  routed `request` uses `is_write_verb` to dispatch; explicit `request_write`
  / `request_read(args, consistent: bool)` for tighter control. Replica
  fallback to primary when fleet empty or `consistent = true`.
- **Live-state plumbing**: process-global `replica_state` (senders + runners
  + upstream slot) so `REPLICAOF` can spawn/swap at runtime;
  `Commands::on_replication_view` hook publishes per-tick offset + connected
  count to the command layer.

### Anti-scope (locked, do not file issues for these in v1.18)

multi-master / cross-DC active-active / CRDTs / Raft / online resharding /
gossip discovery / AUTH / TLS / chain replication / non-RESP wire format for
replication. Automatic quorum failover (`kevy-elect`) is Phase 1.5 — **not**
in v1.18.

### Performance

Single-machine cluster perfgate on lx64 (Debian 13.1, 6.12 kernel, 16
hw threads) — all 6 baseline indicators PASS at the × 0.92 floor;
three of them exceed the recorded baseline outright. Replication
landing did NOT regress non-replication throughput on either reactor.
Reproduce with `bash bench/perfgate.sh <KEVY_BIN>`.

### v1.18 has no carved-out simplifications

Every follow-up the v3-cluster plan originally tracked as "lands in
v1.19+" was actually completed in v1.18: replica peer-addr capture
(T1.28.5), backlog watermark eviction (T1.22.5), background
snapshot serialization (T1.23.5), io_uring + replication (T1.12.5).

### Documentation

- New [`docs/replication.md`](docs/replication.md) — server + client
  recipes, REPLICAOF lifecycle, backlog tuning, simplifications + follow-ups.
- [`docs/cluster.md`](docs/cluster.md) extended with a read/write split
  section showing how cluster mode composes with replication.
- README v3-cluster section.

### Tests

937 workspace tests passing, 0 failures. Highlights:

- `crates/kevy/tests/replication.rs`: full handshake + streaming + snapshot-
  ship round trip + dynamic REPLICAOF lifecycle.
- `crates/kevy-cluster-rw/tests/rw_split.rs`: 1-primary + 2-replica
  ReadWriteClient matrix across every redis-type, READCONSISTENT, reconnect-
  within-backlog (no snapshot), reconnect-outside-backlog (snapshot).

## [kevy-client v1.9.0] — 2026-06-15

Independent `kevy-client` minor (workspace stays at 1.17.0): a **cluster-aware
client**, the ceiling fix for the multi-shard network tail latency a mailrs
dogfood run flagged.

### Added

- **`ClusterClient`** — discovers the topology via `CLUSTER SLOTS`, opens one
  connection per shard, and routes every key to its owner shard by CRC16 slot,
  so no command pays the server-side cross-shard forwarding hop. Requires the
  server in cluster mode (`--cluster`). Covers the standard surface: string
  (set/set_with_ttl/get/incr/incr_by/expire/persist/ttl_ms), hash/list/set/
  zset, multi-key del/exists (routed per key), keyspace-wide dbsize/flushall
  (the server fans these out internally), and ping/publish.

  Measured on a clean 16-core box (server cores 0-3, client cores 8-15):
  **conc64 533k ops/s @ p99 260µs**, vs a single shard's 333k @ 3858µs — 1.6×
  the throughput and a 15× tighter tail, by skipping the forwarding hop. The
  hop, not co-location or thread migration, was the dominant cost (each ruled
  out by measurement on the 4-vCPU dogfood box and the 16-core box).

## [v1.17.0] — 2026-06-14

Minor release: **network `INFO` observability** — the Memory, Keyspace, and
Stats sections now report the whole process rather than the single shard that
happened to answer, plus an API-naming footgun fix. Both from a mailrs dogfood
run of the kevy-server role. Workspace 1.16.0 → 1.17.0; kevy-embedded 1.1.20 →
1.2.0; kevy-client 1.7.16 → 1.8.0 (the `flush` → `flushall` rename below).

### Added

- **`INFO` cross-shard aggregation.** The server runs one independent store per
  shard and answers `INFO` on whichever shard the connection landed on, so the
  Memory / Keyspace / Stats numbers previously reflected ~1/Nth of the process
  (the same single-shard-view trap `DBSIZE` avoids by fanning out). A
  process-wide per-shard stats registry now lets `INFO` sum every shard's slot:
  - **`# Memory`** — `used_memory`, `used_memory_peak`, `evicted_keys` summed
    across shards (was a single shard's slice, often `0`).
  - **`# Keyspace`** — `db0:keys=N,expires=M,avg_ttl=0` (was empty).
  - **`# Stats`** — `total_commands_processed`, `total_connections_received`,
    `instantaneous_ops_per_sec` (Redis-style ring sampled over a ~1.6 s
    window), and `expired_keys` (all were stubbed `0`).
  Each shard publishes its gauges on the reactor tick and bumps command /
  connection counters in the hot path (one thread-local increment, atomics
  written only on the tick); the answering shard freshens its own slot from the
  live store first, so it is never stale.
- **`Store::expires` O(1) counter** — a live count of TTL-carrying keys backing
  `INFO keyspace`'s `expires=`, maintained at every TTL transition rather than
  an O(n) keyspace scan. A drift-guard test asserts it never diverges from the
  O(n) ground truth.

### Changed

- **`flush()` → `flushall()`** across `kevy_store::Store`,
  `kevy_embedded::Store`, and `kevy_client::Connection`. The old name read like
  `Write::flush` (sync-to-disk) but implemented Redis `FLUSHALL` (wipe every
  key + log it) — a data-loss footgun that cost a downstream debugging cycle.
  The new name matches the Redis command; a `#[deprecated]` `flush()` alias
  forwards for one release so callers migrate without a hard break.

## [v1.16.0] — 2026-06-12

Minor release: **COW persistence** — snapshot/rewrite serialization no
longer stalls a shard for the disk write (an O(n)-shallow view freeze,
~8 ns/entry, replaces it), plus an internal steel-dedup pass (one
crash-safe reshard engine shared by server and embedded), an embedded
durability fix, and real `INFO persistence` fields. Workspace 1.15.0 →
1.16.0; kevy-embedded 1.1.19 → 1.1.20; kevy-client 1.7.15 → 1.7.16 (dep
refs only). Perfgate PASS on every unit (6/6 angles, lx64; see
"Changed" for the gate-methodology update).

### Added

- **Background `BGSAVE` / `BGREWRITEAOF`**: the shard freezes a
  copy-on-write view of its keyspace (collection values are
  refcount-shared; mutations copy on write while a snapshot is in
  flight) and a per-shard background thread serializes it. `+OK`
  returns at the freeze; the snapshot/rewritten log swaps in within a
  tick (~100 ms) of the disk write finishing. One job in flight per
  shard (the Redis single-bgsave discipline). `SAVE` keeps its
  synchronous, blocking-durable contract — and is skipped with a log
  line if it races an in-flight background job.
- **`INFO persistence` real fields**: `aof_rewrite_in_progress` now
  reports the answering shard's actual state (it was a stubbed `0`),
  and the new `aof_rewrites_total` counts completed rewrites — the
  completion signal for the now-asynchronous BGREWRITEAOF. Refreshed
  per reactor tick.
- **`kevy_store::Store::collect_snapshot` / `SnapshotView`** (embedded /
  library users): an O(n)-shallow, `Send` point-in-time view —
  serialize on any thread while the store keeps mutating.
  `kevy_persist` serializers accept either a live store or a view
  (`SnapshotSource`).

### Changed

- **`BGSAVE` resets the AOF at the snapshot point** (replacing the old
  save-then-truncate): the new log carries exactly the post-snapshot
  writes, teed while the background save ran. Crash exposure is
  unchanged — the old log keeps receiving every write until the swap,
  and the snapshot-rename + log-swap commit happens in one adjacent
  critical section.
- **Embedded re-shard output is server-identical**: a shard-layout
  migration now writes per-shard `dump-{i}.rdb` snapshots + fresh AOFs
  (previously rewritten-in-place AOFs), and is crash-idempotent via the
  same `reshard.journal` roll-forward the server uses — a crash
  mid-migration previously lost the migrated state from disk. Backup
  rename failures now propagate instead of being silently ignored.
- **Perfgate methodology** (`bench/perfgate.sh`): each angle now
  measures 3 fresh server instances and gates on the median across
  instances (was 3 rounds against one instance). Instance-to-instance
  spread is the dominant noise axis (±5%); the baseline was re-recorded
  accordingly. Affects contributors only.

### Fixed

- **Embedded `Store::save_snapshot` no longer double-applies history on
  restart**: it never reset the AOF, so a restart with both files
  replayed the full log on top of the snapshot — duplicating
  non-idempotent commands (RPUSH'd elements doubled). It now performs
  the same tee'd log reset as `BGSAVE`; a save that races the
  background auto-rewrite waits it out (bounded) instead of writing a
  snapshot whose log would still double-apply.

### Internal

- One crash-safe reshard engine (`kevy_persist::reshard`) behind both
  the server and embedded migration paths; per-shard persistence file
  names have a single source of truth (`kevy_persist::layout`); the
  epoll/io_uring reactors share one cross-core drain
  (`drain_inbound_core`); the CLUSTER topology emitters share one
  derivation.

### Known limitations

- `BGSAVE` / `BGREWRITEAOF` completion is asynchronous: poll
  `INFO persistence` (`aof_rewrite_in_progress` / `aof_rewrites_total`)
  rather than expecting files to have swapped when `+OK` arrives.
- A collection first mutated while a snapshot is in flight is deep-
  copied at that moment (copy-on-write granularity is the whole
  collection) — a write touching a very large hash/zset during a
  background save pays that copy once.
- Tombstone-PEL, cross-shard XREADGROUP, and cross-slot multi-key
  items carried from v1.15.0 (below).

## [v1.15.0] — 2026-06-11

Minor release: **stream consumer-group / PEL persistence** (closing
v1.14.0's known limitation) plus a crash-safety batch from the v1.14
review. Workspace 1.14.0 → 1.15.0; kevy-embedded 1.1.18 → 1.1.19;
kevy-client 1.7.14 → 1.7.15 (dep refs only). Perfgate PASS on both
features (6/6 angles, lx64).

### Added

- **`XSETID key last-id [ENTRIESADDED n] [MAXDELETEDID id]`** (Redis 7
  shape): overwrite a stream's scalar state. Write-classified
  (AOF-propagated) and keyspace-notifying (class `t`); errors mirror
  upstream ("requires the key to exist", "smaller than the target stream
  top item").
- **Snapshot format v4**: each `OP_STREAM` payload now carries the
  stream's consumer groups — group `last_delivered_id`, consumers with
  `last_seen_ms`, and the full PEL (owner, `delivery_time_ms`,
  `delivery_count`), including tombstone rows for XDEL'd-while-pending
  entries. v2/v3 snapshots still load.

### Fixed

- **Consumer groups / PELs now survive every persistence path** (was the
  v1.14.0 known limitation): snapshots (v4 group section), AOF rewrites
  (`XGROUP CREATE`/`CREATECONSUMER` + one `XCLAIM … TIME t RETRYCOUNT n
  FORCE JUSTID` per live PEL row — full delivery fidelity, upstream's own
  rewrite technique), and reshards (the redistribution path carries
  groups). Previously SAVE-only persistence, BGREWRITEAOF, and layout
  re-shards all dropped group state.
- **AOF rewrite scalar drift**: a stream whose tail (or entirety) had
  been XDEL'd replayed with a rolled-back ID clock — and an empty stream
  (deleted-out or groups-only) vanished from the rewrite entirely. The
  rewrite now re-creates empty streams (`XADD … MAXLEN 0` + the new
  `XSETID`) and restores `last_id` / `entries_added` /
  `max_deleted_entry_id` exactly.
- **Server reshard is crash-idempotent**: new snapshots are written under
  temp names and a durable `reshard.journal` marks the commit point
  before any source file is touched; an interrupted migration is rolled
  forward on the next start. Previously a crash inside the migration
  window left the data dir empty (recovery only by hand from
  `.premigration` backups).
- **io_uring dead-conn block waiters**: EOF / write-error / protocol-
  error now cancels a conn's BLPOP/XREAD waiters immediately instead of
  on the 1/16-throttled reap — a parked waiter on a dead conn could
  consume a pushed element meant for a live client for up to 16
  iterations.
- **Embedded / server data-dir interop**: a meta-less multi-shard dir
  opened by the embedded store at `shards = 1` silently loaded shard 0
  only; the shard count is now inferred and the dir migrated whole.
  Default-named single-shard embedded dirs also record `shards.meta`
  (custom `with_aof_filename` / `with_snapshot_filename` names are a
  documented interop opt-out).

### Known limitations

- AOF **rewrites** drop tombstone PEL rows (pending entries whose stream
  entry was XDEL'd) — they can't be re-created by command replay, and
  kevy's XCLAIM/XAUTOCLAIM treat them as reapable. Snapshots (v4)
  preserve them fully; only XPENDING visibility across a
  rewrite-then-restart is affected.
- Multi-stream `XREADGROUP` across shards executes per shard: if one
  shard errors (e.g. NOGROUP) after another delivered, the deliveries
  stand (visible in XPENDING, reclaimable via XAUTOCLAIM) while the
  client sees the error. Upstream pre-validates; documented trade-off.
- Cross-slot multi-key commands execute (single-machine superset) instead
  of returning `-CROSSSLOT`; keyspace-wide views stay whole-keyspace on
  every port (carried from v1.14.0).

## [v1.14.0] — 2026-06-10

Major release: **single-node CLUSTER mode** (key-aware routing — the last
lever of the perf-ceiling campaign), the full hot-path perf campaign (①
allocator/parse/dispatch, ② reactor notification), cross-shard XREADGROUP,
and a TTL-reaper fix. 8-shard headline moves from ~8.7 M to **30.8 M GET /
22.3 M SET ops/s** (pinned-hashtag angle, lx64). Workspace 1.13.0 → 1.14.0;
kevy-embedded 1.1.17 → 1.1.18; kevy-client 1.7.13 → 1.7.14.

### Added

- **Single-node cluster mode** (`--cluster` / `KEVY_CLUSTER=1` /
  `[cluster] enabled`): keys route by Redis-cluster slot (CRC16 `{hashtag}`
  & 16383, one contiguous range per shard); every shard `i` binds a second
  deterministic listener at `port_base + i` (default `port+1+i`) answering
  wrong-shard keys with `-MOVED`. Stock cluster-aware clients
  (`redis-cli -c`, `redis-benchmark --cluster`, client libraries) discover
  the topology and talk straight to the owning shard — no cross-shard
  forwarding tax. The main SO_REUSEPORT port keeps full proxy-style
  behaviour. `CLUSTER SLOTS / SHARDS / NODES / INFO / MYID / KEYSLOT /
  COUNTKEYSINSLOT` answer with the real topology; `KEYSLOT` matches upstream
  (`foo` → 12182), and a packet capture across a full benchmark run shows
  zero spurious MOVEDs.
- **`shards.meta` v2 + automatic re-shard**: the data dir now records
  (shard count, routing scheme); a mismatch at bring-up re-homes every key
  once, with `.premigration.<ts>` backups. Fixes the server silently
  stranding keys on a `--threads` change (it never wrote a meta), and an
  embedded shrink-to-one bug that could truncate a live AOF.
- **`kevy_hash::crc16` / `key_hash_slot`**: XMODEM CRC16 (compile-time
  tables, slice-by-4) + Redis-cluster hashtag slot mapping.
- **Cross-shard non-blocking multi-stream `XREADGROUP`**: previously only
  the first STREAMS key's shard was read, silently dropping streams owned
  elsewhere; now fans out per stream with group context, PEL updates and
  AOF logging on each owning shard (logged as the single-stream rewrite, so
  per-shard replay is correct).
- Fuzz targets for `shards.meta` parsing (round-trip fixpoint) and
  `key_hash_slot` (slot range + hashtag metamorphic property).

### Changed

- **Hot-path perf campaign** (carried since v1.13.0): ArgvPool zero-malloc
  cross-shard forwarding, SmallReply stack-inline replies, borrowed
  single-pass multibulk parse, tier-1 GET/SET dispatch fast path,
  DispatchMeta resolve-once, single conns-probe pre-dispatch, io_uring
  spin→nap→park idle ladder (idle CPU 6.5 % → 0.7 %), batched
  uring_arm_conns, IORING_OP_TIMEOUT bounded park.
- **SLOWLOG defaults to OFF** (`slowlog-log-slower-than = -1`): the 10 ms
  Redis default cost every command an `Instant::now()` pair (~13-19 % at
  multi-M ops/s). Re-enable with `CONFIG SET slowlog-log-slower-than 10000`.
- **TTL reaper bounds its bucket walk** (`samples × 8` visits per round):
  a TTL-free keyspace previously paid a full-table walk every 100 ms tick
  (measured 6 % of server CPU); sparse-TTL coverage leans on the rotating
  random start + lazy expiry.
- `CONFIG GET` now exposes `save` (empty = no save points), so standard
  tooling (e.g. redis-benchmark's per-node config fetch) stops warning.

### Fixed

- A bare 1-element `XREADGROUP` could panic the receiving shard
  (out-of-bounds argv index); now a clean arity error.
- Cluster port ranges that would overflow u16 are rejected at startup
  (loudly) instead of wrapping onto low ports while CLUSTER SLOTS
  advertises 65536+.
- XREADGROUP-gather write housekeeping derived the stream key by scanning
  for the literal "STREAMS", mis-targeting WATCH/notify when a group or
  consumer is named "streams"; now derived from the fixed rewrite shape.
- Cluster mode with AOF off and an empty dir now still records the layout,
  so a later SAVE + non-cluster restart can't silently strand keys.

### Known limitations

- Stream **consumer groups / PELs are not encoded** into snapshots or AOF
  rewrites (pre-existing): they recover only via original-AOF command
  replay, so SAVE-only persistence, BGREWRITEAOF, and layout re-shards drop
  group state (originals remain in `.premigration` backups). Tracked for an
  upcoming release.
- Cross-slot multi-key commands execute (single-machine superset) instead
  of returning `-CROSSSLOT`; keyspace-wide views stay whole-keyspace on
  every port.

## [v1.13.0] — 2026-06-09

Minor release: **cross-shard keyspace scan** for embedded sharding. Workspace
1.12.0 → 1.13.0; kevy-embedded 1.1.16 → 1.1.17; kevy-client 1.7.12 → 1.7.13.
Reported by mailrs (shard-scan gap blocking `with_shards` adoption).

### Added

- **`Store::collect_keys(pattern, limit)`** — `KEYS`/`SCAN`-glob across **every
  shard**. With `with_shards(n > 1)`, the `with(|s| s.collect_keys(..))` escape
  hatch only saw shard 0, so a glob scan (key bust, metrics gauges) silently
  missed `(n-1)/n` of the keyspace. `collect_keys` is the cross-shard,
  read-locked replacement; identical to the old `with(...)` call when
  `shard_count() == 1`. `limit` bounds the total across shards.
- **`Store::for_each_shard(f)`** — run `f` against each shard's underlying
  `kevy_store::Store` (the cross-shard escape hatch for ops not yet wrapped),
  and **`Store::shard_count()`**. Single-key work still uses `with_key`; globs
  use `collect_keys`.

## [v1.12.0] — 2026-06-09

Minor release: **shared-nothing keyspace sharding for embedded mode** — the
embedded store now scales reads across cores. Workspace 1.11.0 → 1.12.0;
kevy-embedded 1.1.15 → 1.1.16; kevy-client 1.7.11 → 1.7.12.

### Added

- **`Config::with_shards(n)`** — partition the embedded keyspace into `n`
  shared-nothing shards (`hash(key) % n`, the same router the network server
  uses), each an independent lock + keyspace + AOF. Concurrent operations on
  different shards never contend, so a multi-threaded embed consumer scales
  across cores. Measured on a 16-core box (in-memory GET, 10 threads):
  **5.3M ops/s (single mutex, v1.10.0) → 12.5M (RwLock, v1.11.0) → 66.3M
  (16 shards) — 12.5× over the campaign, and positive scaling (21M @1 thread
  → 66M @10) where the unsharded store regressed with thread count.**
  - **Default `n = 1`** — the original single-lock / single-`aof-0.aof` layout,
    zero behavior change, zero migration. Sharding is strictly opt-in.
  - With `n > 1`, persistence uses per-shard `aof-{i}.aof` + a `shards.meta`
    file. The first open at `n > 1` re-shards a legacy single AOF into per-shard
    files (the old file is backed up to `aof-0.aof.premigration.<ns>`); changing
    the shard count re-shards via a temp keyspace. Pub/sub is process-wide
    (handled on shard 0), not sharded.
  - `Store::with_key(key, f)` — the `with` escape hatch routed to a key's shard
    (plain `with` targets shard 0).

## [v1.11.0] — 2026-06-09

Minor release: embedded read-path performance — GET throughput and multi-core
read scaling. Workspace 1.10.0 → 1.11.0; kevy-embedded 1.1.14 → 1.1.15;
kevy-client 1.7.10 → 1.7.11. All measured on a 16-core Linux box.

### Changed

- **GET no longer reads the clock for keys without a TTL.** The per-access read
  path called `is_expired_at(Instant::now())`, evaluating the monotonic clock
  on every access even when the key had no deadline. It now reads the clock
  only in the has-deadline branch. **No-TTL GET ~+51%** (embedded in-memory,
  single thread: 19.1M → 28.9M ops/s).
- **TTL'd-key GET uses a coarse cached clock** (Redis `mstime` model): a clock
  refreshed once per reactor batch (server) / reaper tick (embedded background)
  instead of an `Instant::now()` per access. Writes still stamp deadlines from
  a fresh clock, so deadlines stay exact (a key expires at most one
  refresh-interval late, never early). Opt-in per store — only the server
  reactor and the embedded *background* reaper, which refresh the cache, trust
  it; manual-reaper / bare-`Store` reads a fresh clock so lazy expiry still
  works without an explicit tick. **TTL'd GET ~+62%** (17.7M → 28.7M ops/s),
  now on par with no-TTL GET.
- **Embedded `Store` uses a `RwLock`; GET takes a shared read lock.** A
  multi-threaded embed consumer previously serialized every access on one
  exclusive mutex — throughput *regressed* with thread count (16-core: GET
  20.0M @1 thread → 5.3M @10). GET now takes a read lock + a non-mutating
  lookup (when `maxmemory == 0`), so concurrent readers run in parallel:
  **10-thread GET 5.3M → 12.5M ops/s (+136%)**. Expired keys are reclaimed by
  the active reaper / next write rather than lazily on read (read returns
  `None` either way); with eviction on, GET keeps the exclusive + LRU-stamping
  path.

### Added

- `cargo run -p kevy-embedded --example bench_embed[_mt]` — single- and
  multi-threaded in-process throughput harnesses.

## [v1.10.0] — 2026-06-09

Minor release: the embedded auto-AOF-rewrite is now **non-blocking**, plus a
push-style metric callback — closing the two gaps left from the mailrs feedback
(`kevy-product-feedback-2026-06-09`). Workspace 1.9.0 → 1.10.0; kevy-embedded
1.1.13 → 1.1.14; kevy-client 1.7.9 → 1.7.10.

### Changed

- **Embedded background auto-AOF-rewrite no longer blocks application writes.**
  v1.9.0 ran the auto-rewrite inline under the store lock (blocking writers for
  the full serialize + disk write + fsync). It now runs in three phases: (1)
  serialize the keyspace to memory under the lock and start teeing live appends
  into a diff buffer, (2) **release the lock** and spill the snapshot image to
  disk + fsync — the expensive part, off the hot path, (3) re-take the lock
  briefly to append the tee'd diff and atomically swap the file in. Writes
  during the disk spill are captured by the tee, so nothing is lost; crash
  safety is unchanged (atomic `rename`). The manual `Store::rewrite_aof()` stays
  synchronous (the explicit "rewrite now" path); a manual call is a no-op while
  a background rewrite is in flight.

### Added

- **`Config::with_metric_sink(callback)`** — a push-style metric callback that
  fires `KevyMetric::Replay { commands, bytes, elapsed_ms }` after startup AOF
  replay and `KevyMetric::Rewrite { keys, before_bytes, after_bytes,
  elapsed_ms }` after each AOF rewrite. For continuous monitoring without
  polling `info()`. `KevyMetric` is `#[non_exhaustive]`.

## [v1.9.0] — 2026-06-09

Minor release: AOF maintenance + observability for embedded mode, from the
mailrs production feedback (`kevy-product-feedback-2026-06-09`). Workspace
1.8.1 → 1.9.0; kevy-embedded 1.1.12 → 1.1.13; kevy-client 1.7.8 → 1.7.9.

### Added

- **Automatic AOF rewrite in embedded mode.**
  `Config::with_auto_aof_rewrite(pct, min_size)` triggers a `BGREWRITEAOF`-style
  compaction when the live AOF has grown `pct` percent past its size at the
  previous rewrite and is at least `min_size` bytes — defaults **100 % /
  64 MiB**, matching Redis and the network server. The check rides the
  background reaper tick (or `Store::tick` in manual reaper mode). The manual
  `Store::rewrite_aof()` already existed and is unchanged.
- **Embedded introspection API.**
  `Store::info() -> KevyInfo` (keys, used_memory, aof_bytes, expire_pending,
  evictions, expired_keys), `Store::expire_pending_count()` (live keys carrying
  a TTL — the expire-set size), and `Store::ttl(key) -> Option<Duration>` (an
  ergonomic wrapper over the raw `ttl_ms` PTTL sentinels). Backed by a new
  `kevy_store::Store::ttl_pending_count()`.
- **`docs/persistence.md`** — AOF / snapshot / fsync policy / TTL semantics /
  rewrite & compaction / crash recovery / file-naming / embedded introspection,
  in one place. Linked from the README.

### Changed

- **AOF replay now logs its wall-clock time**: `… replayed N commands from M
  bytes in T ms (clean)`. Replay time scales with the AOF, so surfacing it
  gives operators a baseline to watch.

## [v1.8.1] — 2026-06-09

Patch release: **TTL deadlines now survive a restart.** Workspace 1.8.0 →
1.8.1; kevy-embedded 1.1.11 → 1.1.12; kevy-client 1.7.7 → 1.7.8. Reported by
the mailrs production deployment (INC-2026-06-09).

### Fixed

- **A key's TTL was reset to a fresh full duration on every restart.** TTL was
  persisted as a *relative* `PEXPIRE <ms>` in the AOF (and as remaining-ms in
  the binary snapshot), so AOF replay / snapshot load re-anchored the deadline
  to load-time. A key set with a 300 s TTL, after a restart hours later, came
  back with a fresh 300 s instead of expiring at its original instant — so a
  cache entry could outlive its intended lifetime indefinitely across frequent
  restarts (it never expired from the reader's point of view). In-memory TTL
  (within a single process lifetime) was always correct; only persistence was
  affected.
  - **All persistence paths now record an absolute Unix-ms deadline.** The
    embedded `set_with_ttl`/`expire` log `PEXPIREAT`; the server's AOF append
    follows a relative TTL write (`EXPIRE`/`PEXPIRE`/`SETEX`/`PSETEX`/
    `SET … EX|PX`) with an absolute `PEXPIREAT` correction; `BGREWRITEAOF`
    emits `PEXPIREAT`; the binary snapshot stores the absolute deadline
    (format v3). Load/replay subtracts elapsed wall-clock and drops keys whose
    deadline already passed.
  - Backward-compatible: a v2 snapshot (relative TTL) and old relative
    `PEXPIRE` AOF entries still load (treated as relative-from-load, the prior
    behaviour) — no migration needed; new writes are absolute.

### Added

- **`EXPIREAT` / `PEXPIREAT` commands** (absolute Unix-time expiry, matching
  Redis). Single-key routed; replicated to the AOF. These are also the wire
  form the persistence layer now uses internally.

## [v1.8.0] — 2026-06-07

Minor release: io_uring is now the default reactor on Linux, with an
automatic epoll fallback. Workspace 1.7.0 → 1.8.0; kevy-embedded 1.1.10 →
1.1.11; kevy-client 1.7.6 → 1.7.7.

### Changed

- **The Linux reactor now auto-selects io_uring at startup, falling back
  to epoll when the host can't build a ring.** Previously io_uring was
  opt-in via `KEVY_IO_URING=1`; epoll was the default. Now an unset
  `KEVY_IO_URING` probes io_uring (creates + drops a real ring with the
  production parameters, including the buffer-ring registration) and uses
  it when available — otherwise it logs the reason and uses epoll.
  **Startup never fails over reactor choice.** This catches a
  seccomp-blocked `io_uring_setup` (Docker's default profile) and
  pre-5.19 kernels before any shard loads data.
  - Override still honoured: `KEVY_IO_URING=0/off/no/false` forces epoll;
    any other value forces io_uring with no fallback (a setup failure then
    surfaces loudly — for benchmarks / tests).
  - The startup line reports the choice: `kevy: reactor = io_uring
    (io_uring available)` or `... = epoll (io_uring unavailable …)`.

### Fixed

- **io_uring disconnect leaked block waiters and pub/sub registrations.**
  The io_uring reactor's connection reaper hand-rolled its teardown
  (removed the conn + unsubscribed channels only), skipping the shared
  `close_conn` path the epoll reactor uses. So disconnecting a connection
  that was parked on a cross-shard `BLPOP`/`XREAD` left its arbiter waiter
  and `psub` registrations behind — a later `RPUSH`/`XADD` could wake the
  gone waiter and consume an element meant for a live client. The reaper
  now routes through `close_conn` (which runs `drop_for_conn`,
  `cancel_xshard_on_close`, channel + pattern unsubscribe). Only reachable
  under io_uring; epoll was always correct.

## [v1.7.0] — 2026-06-07

Minor release: cross-shard multi-stream `XREAD`. Workspace 1.6.1 → 1.7.0;
kevy-embedded 1.1.9 → 1.1.10; kevy-client 1.7.5 → 1.7.6.

### Fixed

- **Non-blocking `XREAD … STREAMS s1 s2 …` over streams on different shards
  returned partial data.** It routed to the first STREAMS key's shard only,
  so streams owned by other shards were silently dropped (no error). It now
  fans each stream out to its owning shard and merges the replies in request
  order — empty streams skipped, `*-1` when all empty, `COUNT` applied per
  stream, `$` resolved on each stream's owning shard. Single-stream `XREAD`
  keeps the fast single-shard path; blocking `XREAD` already parks on the
  origin shard via the cross-shard BLOCK arbiter (v1.5.0).
  - `XREADGROUP` multi-stream cross-shard remains a follow-up (its `>`
    consume semantics need separate handling).
  - Additive internal API only (a new `Route::XReadGather` variant); no
    public breakage.

## [v1.6.1] — 2026-06-07

Patch release: faster snapshots. Workspace 1.6.0 → 1.6.1; kevy-embedded
1.1.8 → 1.1.9; kevy-client 1.7.4 → 1.7.5. No public API change.

### Changed

- **Snapshot / BGREWRITEAOF bulk writes use a 1 MiB BufWriter** (was the
  8 KiB default). `SAVE` was measured at only ~12 % of disk sequential
  bandwidth (758 MB/s vs a 6.1 GB/s NVMe ceiling on an M4 Pro) — the small
  buffer turned a multi-hundred-MB snapshot into tens of thousands of small
  `write(2)`s. The larger buffer lifts SAVE to **~1.73 GB/s (+128 %)**.
  Content is byte-identical; only the flush granularity changes.

## [v1.6.0] — 2026-06-07

Minor release: AOF `appendfsync always` group commit. Workspace 1.5.1 →
1.6.0; kevy-embedded 1.1.7 → 1.1.8; kevy-client 1.7.3 → 1.7.4.

### Added / Changed

- **AOF group commit for `appendfsync always`.** Previously every write
  fsynced individually (`flush()+sync_data()` per command). Now a pipelined
  batch's writes are buffered and fsynced once at the batch boundary — still
  before that batch's replies leave the shard, so the "durable before reply"
  contract is unchanged. Measured **+46 %** (0.89M → 1.30M SET/s, `-c50
  -P16`, 10 shards, lx64 NVMe); the per-write-durable vs 1-second-window
  gap shrank from −39 % to −8 %. Applies to all always-write paths on both
  reactors (epoll + io_uring local reads, and the cross-shard request
  batch). `everysec` / `no` / cache-only paths are unchanged.
  - New public API on `kevy_persist::Aof`: `begin_group()` / `end_group()`
    (additive; existing embedders recompile unchanged).

### Verified

- New `kevy-persist` test `aof_group_commit_defers_then_flushes` (the batch
  is not on disk until `end_group`, then fully durable). Full workspace
  tests + clippy green; compat3 differential 135/135 vs valkey 9.1 + redis
  7.4. Regression A/B (lx64): no GET/SET hot-path change; 3-way still leads
  (kevy io_uring ~2.2× valkey / ~1.7× redis). See `bench/REPORT.md`.

## [v1.5.1] — 2026-06-07

Patch release: three valkey-parity / correctness fixes surfaced by
extending the cross-engine differential harness (`bench/compat3.sh`) to
Streams / Geo / blocking / RENAME — now 135/135 vs valkey 9.1 + redis 7.4,
and gated in CI. All three are pre-existing (not v1.5.0 regressions); no
public API change. Workspace 1.5.0 → 1.5.1; kevy-embedded 1.1.6 → 1.1.7;
kevy-client 1.7.2 → 1.7.3.

### Fixed

- **Cross-shard `RENAMENX` could lose the source key.** When source and
  destination hashed to different shards and the destination already
  existed, step 1 took the source off its shard but the NX-refused step-2
  put was never rolled back — the reply `:0` was correct but the source
  key was gone. The refused put now hands the value back and the
  orchestrator restores it on the source's shard before replying (a new
  `RenameStep::Restore`), so a no-op `RENAMENX` no longer loses data.
- **`XGROUP` / `XINFO` were unusable on a multi-shard server.** Their
  stream key is at `args[2]` (after the subcommand) but they routed by
  `args[1]` (`CREATE`/`STREAM`), landing on the wrong shard — `XGROUP
  CREATE` failed with "key doesn't exist" and `XREADGROUP`/`XACK`
  cascaded. Now routed by the real key (keyless `HELP` forms stay local).
- **`GEOHASH` / `GEOPOS` diverged from valkey in the last digit(s).**
  The 11th `GEOHASH` char spilled the low score bits instead of
  zero-padding like Redis; `GEOPOS` decoded the cell centre with a
  float-op order that rounded differently than Redis's `(min+max)/2`.
  Both now reproduce valkey byte-for-byte. Adds kevy-geo unit tests (the
  existing ones only checked the first 10 geohash chars).

## [v1.5.0] — 2026-06-07

Minor release: cross-shard blocking pops. A `BLPOP` / `BRPOP` / `XREAD
BLOCK` whose key lived on a shard other than the connection's used to hang
the client forever; multi-key `BLPOP` was rejected outright. Both are now
fixed via a cross-shard BLOCK arbiter (`kevy_rt::block_xshard`). New
`Commands` hooks are additive with default bodies, so embedders recompile
unchanged. Workspace bump 1.4.2 → 1.5.0; kevy-embedded 1.1.5 → 1.1.6;
kevy-client 1.7.1 → 1.7.2 (both inherited the workspace bump, no API
change).

### Added

- **Cross-shard blocking pops (v2-7e).** `BLPOP` / `BRPOP` / `XREAD BLOCK`
  / `XREADGROUP BLOCK` now work when watched keys live on shards other
  than the connection's, and multi-key `BLPOP k1 k2 …` is supported
  (previously rejected). The connection parks on its origin shard and
  watch registrations fan out to each key's owning shard; the origin is
  the sole arbiter, so no target shard ever pops speculatively (which
  would lose data when two keys go ready at once). See
  `kevy_rt::block_xshard`. New additive `Commands` hooks
  (`block_serve_argv`, `block_ready`, `wake_idx`) default to no-op, so
  embedders recompile unchanged.

### Fixed

- A single-key `BLPOP` / `BRPOP` / `XREAD BLOCK` whose key hashed to a
  shard other than the connection's **hung the client forever** — the
  command was forwarded to the key's shard as a plain dispatch, which on
  an empty list returned a 0-byte reply and never parked, woke, or timed
  out. Now it parks correctly via the cross-shard arbiter. Regression
  test `blocking_cross_shard::blpop_remote_key_times_out_not_hang`
  (nshards = 8).

### Known gaps

- Non-blocking multi-stream `XREAD` across shards still reads only the
  first STREAMS key's shard (a missing-feature, not a hang) — a separate
  cross-shard gather, tracked for a follow-up.

## [v1.4.2] — 2026-06-07

Patch release rolling up the v1.4.1 follow-ups: an XREAD BLOCK bug fix,
two CI/release hardening jobs that catch the exact failure modes the
v1.4.0 → v1.4.1 sequence exposed, and a workspace-wide src/*.rs ≤ 500
LOC sweep (every production file now matches the CLAUDE.md house rule;
test files exempt per Rust community norm).

No public API breaks. New trait method `Commands::resolve_block_argv`
on `kevy-rt` is additive with a default body, so existing embedders
recompile unchanged.

### Fixed

- `XREAD BLOCK ms STREAMS key $` no longer hangs when an `XADD` lands
  during the park window. The previous implementation kept the literal
  `$` in the parked argv; the wake retry re-resolved `$` to the
  *post-`XADD`* `last_id`, so the just-added entry sat at the cursor
  and the read returned 0 rows (the conn timed out instead of
  receiving the entry it was supposed to be woken by). Park-time now
  rewrites each `$` to the stream's current `last_id` via a new
  `Commands::resolve_block_argv` hook, so the wake retry sees the
  original cursor and the freshly added entry. New regression test
  `xread_block_dollar_id_wakes` exercises the real `$` form;
  `xread_block_woken_by_concurrent_xadd` keeps documenting the
  explicit-ID variant. (ROADMAP task #10 / v2-7d known limitation,
  closed.)

### Added — CI / release plumbing

- `.github/workflows/ci.yml`: new `release-profile` job that runs
  `cargo test --workspace --release --lib --tests` on every push to
  `release/**` and `hotfix/**` branches. Catches release-only bugs
  (compiler eliminating a branch, sub-microsecond timings rounding
  to zero — the exact shape of the v1.4.0 SLOWLOG regression) at PR
  review time instead of inside the publish workflow.
- `.github/workflows/release.yml`: new `Publish chain self-check`
  step before the publish loop. Reads `cargo metadata --no-deps`,
  lists every workspace member whose `publish` field is unset, and
  diffs that set against the hand-maintained `for c in …` chain.
  Aborts on either side of the symmetric difference: a publishable
  crate not in the loop (the v1.4.0 release shipped without
  kevy-geo this way), or a name in the loop that isn't a publishable
  workspace member.

### Changed — internal refactor (no API surface)

- All production `src/*.rs` files now ≤ 500 LOC and every `fn` ≤ 50
  LOC, matching the CLAUDE.md house rule. Test files (`tests.rs`
  modules) are exempt per the Rust community norm and remain
  uncapped.
- New sibling modules carry the lifted-out code; each keeps its
  parent's `impl<C: Commands> Shard<C>` (or `impl Commands for
  KevyCommands`) so behaviour + call shape are unchanged:
  - `kevy-rt/src/exec_dispatch.rs` — `start_single` +
    `try_inline_local` + the new `park_blocked` /
    `post_write_housekeeping` / `dispatch_inline` helpers that bring
    `try_inline_local` from 106 LOC down to 35 LOC.
  - `kevy-rt/src/shard_tick.rs` — per-tick housekeeping
    (`apply_live_runtime_config`, `maybe_auto_rewrite_aof`).
  - `kevy/src/cmd_resolve.rs` — `KevyCommands::resolve`'s body as
    `kevy_resolve(args)` + a `route_for_verb(upper, args)` helper.
  - `kevy/src/dispatch_resp3.rs` — `try_resp3_overrides` + the four
    `emit_*_resp3` reply helpers.
  - `kevy-client/src/subscribe_io.rs` — `send_to` / `recv_remote` /
    `frame_to_event` / `classify` and the per-field reply unwraps.
  - `kevy-config/src/error.rs` — `ConfigError` enum + Display +
    Error impls; the public `kevy_config::ConfigError` path is
    unchanged.
  - `kevy-embedded/src/pubsub_bus.rs` — `BusEntry` + `PubsubBus`
    (the per-`Inner` channel/pattern registry).

### Tooling

- New end-to-end test `xread_block_dollar_id_wakes` in
  `crates/kevy/tests/blocking.rs` (now 12 tests).

## [v1.4.1] — 2026-06-06

Hotfix for v1.4.0's SLOWLOG threshold semantics under release-profile
builds. The v1.4.0 tag exists in git but never reached crates.io —
the release pipeline's `Verify tag builds (release profile)` job
failed in this exact case, and the publish step never ran. v1.4.1 is
the first published `1.4.x` artifact.

### Fixed

- `SLOWLOG`: `slowlog-log-slower-than 0` now records every command,
  including the sub-microsecond writes whose `Instant::elapsed().
  as_micros()` rounds to `0` under release-profile optimization.
  Previously the threshold check was `elapsed <= threshold → skip`,
  meaning a `threshold = 0` discarded the `elapsed == 0` row that
  release-profile SETs always produce. The fix is one line in
  `exec_slowlog.rs` (`<=` → `<`) and brings the behavior in line
  with Redis (`if (duration < slowlog_log_slower_than) return;`).
  Caught by the v1.4.0 release pipeline; covered by all four
  `slowlog_*` integration tests under `--release`.

## [v1.4.0] — 2026-06-06

RESP3 wire protocol + the full v2 command sprint: Streams (basic ops +
consumer groups + BLOCK), Geo, BLPOP/BRPOP, keyspace notifications,
SLOWLOG, cross-shard RENAME, CONFIG REWRITE-with-comments, reactor-
tuning knobs. The first release tagged through the new git-flow SOP.

### Added — RESP3

- `HELLO [protover [AUTH user pass] [SETNAME name]]`. `HELLO 3` flips
  the connection into RESP3 mode (per-conn `RespVersion`, threaded
  through every cross-shard `Op::Dispatch`). RESP2 stays the default
  and the hot-path measurements remain unchanged.
- RESP3-shaped replies migrated: `HGETALL` / `CONFIG GET` → Map,
  `SINTER` / `SUNION` / `SDIFF` → Set, `ZSCORE` / `ZINCRBY` → Double,
  `ZRANGE WITHSCORES` → nested `[bulk, double]`, `INFO` /
  `CLIENT INFO|LIST` → Verbatim string, `(P)SUBSCRIBE` message
  frames → Push (`>`). `RESP2` replies for the same commands are
  unchanged.
- `kevy-client`: RESP3 Push-frame demux + `Subscriber::hello3()` so
  embedders can negotiate RESP3 from a clean async API.

### Added — Streams (v2-7)

- Basic ops: `XADD` / `XLEN` / `XRANGE` / `XREVRANGE` / `XDEL` /
  `XTRIM` / `XREAD`. New `Value::Stream(Box<StreamData>)` keeps the
  Value enum at 32 bytes — the indirection only pays on stream
  operations.
- Consumer groups: `XGROUP CREATE|SETID|DESTROY|CREATECONSUMER|
  DELCONSUMER`, `XREADGROUP`, `XACK`, `XPENDING`, `XCLAIM`,
  `XAUTOCLAIM`. PEL stored in a `BTreeMap<StreamId, PelEntry>` so
  `XPENDING start end` is `O(log n + k)`; per-consumer `pel_count` is
  maintained on every PEL mutation so `XINFO` runs in O(group size).
- `XINFO STREAM | GROUPS | CONSUMERS | HELP`.
- `t`-class keyspace notifications (matches Redis): XADD / XDEL /
  XTRIM / XGROUP* / XACK / XCLAIM / XAUTOCLAIM / XREADGROUP all fire
  their lowercased verb name. The `A` flag includes the `t` class,
  matching modern Redis.
- AOF rewrite for streams: one `XADD` per entry (correct but linear
  in stream size — documented for now). RDB has a dedicated
  `OP_STREAM = 6` opcode carrying the full scalar state
  (`last_id`, `max_deleted_id`, `entries_added`).

### Added — BLOCK reactor (v2-7d)

- Per-shard `BlockedClients` registry shared by `BLPOP` / `BRPOP` /
  `XREAD BLOCK` / `XREADGROUP BLOCK`. FIFO per key (Redis arrival
  order), secondary index by conn for O(1) cleanup on close. Empty
  in steady state so the wake / tick hot paths short-circuit on
  `is_empty()`.
- New `Commands::block_hint(args) -> BlockHint` trait method (default
  `None`), folded into `ResolvedCmd { block_hint, wake_idx }` so the
  verb table is scanned once per command. The reactor's wake hook
  fires only when `wake_idx` is `Some` *and* `BlockedClients` is
  non-empty — so the steady-state cost of the registry on a
  no-block workload is one `is_empty()` check per write.
- `BLPOP key timeout` / `BRPOP key timeout` (single-key form). Empty
  list parks the conn; a sibling `LPUSH` / `RPUSH` wakes the oldest
  waiter and replays the command. Multi-key form returns an explicit
  cross-shard error (v2-7e will lift the same-shard subset).
- `XREAD BLOCK ms STREAMS key id` / `XREADGROUP GROUP g c BLOCK ms
  STREAMS key >`: same-shard waiter on the first STREAMS key, woken
  by an `XADD` to that key. `XREADGROUP BLOCK` only parks for
  `>`-mode streams (matches Redis).
- 11 end-to-end blocking tests against a real reactor + socket
  (hit / timeout / wake per command).

### Added — Geo (v2-6)

- `GEOADD` / `GEOPOS` / `GEODIST` / `GEOHASH` — stored as a ZSet with
  a 52-bit interleaved geohash for the score. `GEOHASH` emits the 11-
  char base32 form (the 11th char carries an IEEE-754 LSB drift; the
  first 10 chars match Redis exactly).
- `GEOSEARCH FROMLONLAT|FROMMEMBER BYRADIUS|BYBOX` + the legacy
  `GEORADIUS` / `GEORADIUSBYMEMBER` family + `GEOSEARCHSTORE`. All
  share one `run_search` core using 9-cell neighbor pruning plus
  exact Haversine secondary filtering.

### Added — Ops + config (v2-1 → v2-5)

- Keyspace notifications: per-shard `NotificationFlags`, hot-reloaded
  from the `[notify]` config section (`notify-keyspace-events Kg$`-
  style flag string). Single-key writes notify in the `try_inline_
  local` fast path; multi-key writes route through dedicated
  `maybe_notify_*` hooks.
- `[advanced]` config section (`spin_limit` / `park_timeout_ms` /
  `tick_check_every`) — the old hardcoded SPIN_LIMIT / PARK_TIMEOUT_
  MS / TICK_CHECK_EVERY constants are now per-shard fields, threaded
  through `Runtime::with_advanced`. Defaults match the pre-v1.4 hot
  numbers.
- `RENAME` / `RENAMENX` cross-shard orchestrator using
  `take_with_ttl` + `put_with_ttl` (same-shard atomic still goes
  through one `Store::rename`).
- `SLOWLOG GET | LEN | RESET | HELP` — bounded ring of slow
  command records per shard, hot-reloaded from
  `[slowlog].slower_than_micros` + `[slowlog].max_len`. SLOWLOG OFF
  (default) skips the clock pair entirely on the hot path.
- `CONFIG REWRITE` now preserves comments + key ordering (line-by-
  line rewrite, not a syntax-tree rebuild; missing sections get
  inline-appended).

### Changed

- `kevy-rt::Commands::resolve` now produces a `ResolvedCmd` with two
  new fields: `block_hint: BlockHint` and `wake_idx: Option<u8>`.
  **Breaking** for any external `impl Commands for X` that
  constructs a `ResolvedCmd` literal — add the two fields. The
  default `resolve()` implementation (which calls the per-attribute
  methods one-by-one) does so automatically.
- `BlockHint` / `BlockKind` re-exported from `kevy-rt` so concrete
  command-set crates (kevy + future ports) can return blocking
  classifications without taking a kevy-rt-internal dependency.
- Reply ordering: `Conn.blocked: bool` gates command dispatch on
  parked conns; the reactor stops parsing further commands on a
  conn while it's blocked, resumes on wake / timeout.
- CI workflows: `ci.yml` triggers expanded from `[main, develop]`
  (the `main` branch never existed in this repo) to `[master,
  develop, feature/**, release/**, hotfix/**, bugfix/**,
  support/**]` — feature branches now run CI on every push so
  Linux-specific build issues are caught before the merge.
- `master` is now the v1.3.0 ref (was: initial commit). All v1
  tags previously landed on `develop`; future releases follow the
  git-flow SOP and tag on `master` via `release/*` branches.

### Fixed

- `io_uring` reactor compile-clean on Linux:
  `crate::shard::TICK_CHECK_EVERY` was renamed to a per-shard field
  (`self.tick_check_every`) in v1.4 (advanced config), and the
  io_uring path's `Inbound::RequestBatch` drain was missing the
  `RespVersion` argument that v2-7 added to `Op::Dispatch`. macOS
  builds didn't notice because the io_uring path is
  `#[cfg(target_os = "linux")]`. CI now covers Linux on every push.

### Tooling

- New `GIT-FLOW.md` codifies the feature / release / hotfix flows
  including the v2-7d retro lessons (push the feature branch once,
  squash-merge on finish, bump versions on release branches only).
- New `.githooks/pre-commit` rejects any commit whose staged
  `crates/*/src/**/*.rs` blob exceeds 500 LOC (test files exempt).
  Set up via `bash .githooks/install.sh`, which also wires
  `gitflow.feature.finish.squash = true`.
- New `crates/kevy/tests/blocking.rs` — 11 end-to-end blocking
  tests for BLPOP / BRPOP / XREAD BLOCK / XREADGROUP BLOCK.

## [Unreleased]

The `develop` branch's snapshot that became the `v1.0.0-rc` line.
Everything below is already on `develop`.

### Added — Wave 3: embedded + WASM + release plumbing

- **New crate `kevy-embedded`** ([`crates/kevy-embedded/`](crates/kevy-embedded/)):
  in-process Redis-compatible KV without the server/runtime. Optional
  AOF + snapshot persistence, optional eviction (all 8 policies from
  Wave 2), optional background TTL reaper thread (or caller-driven
  `Store::tick()` for WASM). Zero crates.io deps — depends only on
  `kevy-store` + `kevy-persist`. 16 unit tests + 2 examples.
- **`kevy-bytes` builds on `wasm32-unknown-unknown`** — `SmallBytes`
  now has a cfg-gated 32-bit `Heap` layout
  (`ptr + len(u32) + cap(u32) + pad + tag`) alongside the existing
  64-bit `ptr + len + cap_and_tag × usize` shape. 64-bit perf is
  unchanged (locked layout, release perf_gate budgets met).
- **`kevy-embedded` + transitive closure** compile clean for
  `wasm32-unknown-unknown` AND `wasm32-wasip1`. See
  [`docs/wasm.md`](docs/wasm.md) for browser / WASI / Cloudflare
  Workers walkthrough.
- **GitHub Actions CI** ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)):
  x86_64-linux + aarch64-darwin (M-series) test matrix, wasm32 cargo
  check, nightly miri on `kevy-map` + `kevy-bytes`, vs-valkey docker
  smoke. Release pipeline (`release.yml`) runs `cargo publish
  --dry-run` for every publishable crate in dependency order and
  drafts a GitHub release on `vX.Y.Z` / `-rcN` / `-betaN` tags.
- **v1.x stability commitment** in [`README.md`](README.md): persistence
  format, RESP wire protocol, public Rust API, CLI flags + env vars,
  TOML schema, eviction policy names + algorithms — all add-only
  across v1.x.

### Added — Wave 2: 防 OOM + 防数据丢

- **`maxmemory` + 8 eviction policies**
  (`noeviction` / `allkeys-{lru,lfu,random}` / `volatile-{lru,lfu,random,ttl}`).
  Sample-based with `maxmemory-samples = 5` (matches Redis); LFU uses
  log-scale increment with splitmix32-derived PRNG (no decay in v1.0).
  Per-entry weight cache + `ENTRY_OVERHEAD` constant give O(1)
  accounting on every mutation path. Unlimited mode (`maxmemory = 0`,
  the default) stays at its tuned hot-path budget.
- **Active TTL reaper** — `Store::tick_expire(samples, rounds)` runs
  Redis's `activeExpireCycle` per shard. The reactor calls it at the
  configured `[expiry].hz` (default 10 Hz / 100 ms) via the new
  `Commands::on_shard_tick` hook in `kevy-rt`. Lazy expiry still
  runs alongside.
- **`BGREWRITEAOF`** — `Aof::rewrite_from(&Store)` dumps current state
  to `<aof>.rewrite` as canonical SET/HSET/RPUSH/SADD/ZADD (+ PEXPIRE
  for TTL'd keys) and atomically `rename(2)`s over the live AOF. v1.0
  is synchronous (each shard blocks for its own rewrite); v1.x will
  incrementalise. Auto-triggered by the shard tick when the AOF grew
  ≥ `auto_aof_rewrite_percentage %` (default 100) above its size at
  the last rewrite AND is ≥ `auto_aof_rewrite_min_size` (default 64 MiB).
- **`appendfsync` wired from config** — `Always` / `EverySec` (default)
  / `No`. Existing fsync semantics in `kevy_persist::Aof` were
  already implemented; this commit just plumbs the choice from
  `cfg.persistence.appendfsync` through to the per-shard `Aof::open`.
- **Crash-safety contract** documented in
  [`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md): truncated
  AOF tails replay cleanly (covered by
  `aof_truncated_tail_is_tolerated_on_restart`), snapshot+AOF load
  order is snapshot-first / replay-second. Power-loss simulation
  harness at [`bench/crash-test.sh`](bench/crash-test.sh).
- **`MEMORY USAGE / STATS / DOCTOR / PURGE`** commands; `INFO memory`
  now surfaces live `used_memory`, `used_memory_peak`,
  `evicted_keys`, `maxmemory_human`.

### Changed

- `kevy_persist::Fsync` now derives `Debug` / `PartialEq` / `Eq`
  (Wave 3 #5 needed it for `Config::default()` to derive Debug).
- `kevy_persist::Aof` carries its own path + size estimates so
  auto-rewrite can compute the trigger threshold without `fstat()`
  per append.
- `kevy_rt::Commands` trait gained two hooks (default no-op):
  `on_shard_init(store)` lets per-shard config (e.g. maxmemory) land
  before the reactor starts; `on_shard_tick(store)` +
  `shard_tick_interval_ms()` drive the active TTL reaper at the
  configured cadence.
- `kevy_map::KevyMap` gained `iter_from_bucket(start)` for the
  eviction sampler's random-start window. Existing `iter()` unchanged.

### Fixed

- `kevy-embedded::Store::Drop` recovers from mutex poison so the
  final AOF flush always runs (a panic in some method during the
  session shouldn't strand the EverySec window's writes).
- Several clippy lints across `kevy-map` / `kevy-store` / `kevy-persist`
  / `kevy-embedded` (collapse `if let`, type alias for complex
  signatures, `.is_multiple_of`, `io::Error::other`) so CI's
  `cargo clippy --workspace -- -D warnings` runs clean on first push.

---

## [v1.0.0-w1] — 2026-05-28

Wave 1 close: config + ops + docs. See git tag for the full list;
headlines:

- New crate `kevy-config` — 0-dep TOML subset parser + Config schema.
- 13 ops commands: `INFO` / `CLUSTER * ` / `DEBUG SLEEP` / `WAIT` /
  `SHUTDOWN` / `CONFIG GET/SET/REWRITE/RESETSTAT` / `CLIENT *`.
- Top-level `README.md` + `MIGRATION-FROM-VALKEY.md` (94-cmd
  parity table).
- Code-quality rule: `src/*.rs ≤ 500 LOC` / `fn ≤ 50 LOC` codified
  as a project coding rule.

## [v0.1.1-deep-polish-rc] and earlier

Per-crate perf polish across `kevy-bytes` / `-hash` / `-map` /
`-resp` / `-ring` / `-store`. The five library crates reach noise-floor
parity or better vs the best open-source Rust / Go / C / C++
competitor at each workload.
