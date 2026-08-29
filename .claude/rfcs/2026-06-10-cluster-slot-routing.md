# RFC: single-node CLUSTER slot routing (roadmap ③ key-aware routing)

- Date: 2026-06-10
- Status: accepted (autorun; carrier approved by user 2026-06-10)
- Scope guard: **single machine, single process.** Multi-node distribution,
  failover, MIGRATE/ASK, gossip stay permanently OUT (`scope-decisions.md`).

## Problem

With `--threads 8` all shards accept on one SO_REUSEPORT port; a connection
lands on a random shard, so ~7/8 (87.5 %) of single-key commands are forwarded
over an SPSC ring to the owning shard (~3.5× cost of a local op). A
cluster-aware client that knows key→shard placement could connect to each
shard directly and hit ~100 % local ops. 8sh GET measured ~11.2 M ops/s;
naive ceiling if forwarding disappears ≈ 38 M.

The carrier for key-aware routing is a **read-only subset of the Redis
CLUSTER protocol**: standard clients (`redis-benchmark --cluster`,
`redis-cli -c`, every cluster-aware client library) already implement slot
discovery + per-node connections; we expose each shard as a "node".

## Design decisions

### D1. Slot model

- `key_hash_slot(key) = crc16(hashtag(key)) & 16383` — Redis-standard
  CRC16-CCITT (XMODEM: poly 0x1021, init 0, no reflect/xor), `{hashtag}`
  extraction (first `{`…next `}`, non-empty content, else whole key).
  Check vector: `crc16(b"123456789") == 0x31C3`.
- slot → shard: contiguous even split. `slot_to_shard(slot, n) =
  (slot * n) >> 14` (16384 = 2¹⁴ — multiply + shift, no division). Shard *i*
  owns `[ceil(i·16384/n), ceil((i+1)·16384/n))`; the floor inverse above is
  exact for these boundaries. CLUSTER SLOTS emits one contiguous range per
  shard.
- Routing scheme is a **startup-time property of the data dir + config**, not
  hot-settable. `shard_of` keeps the `n == 1` short-circuit; the slots path
  replaces KevyHash when cluster mode is on.
- CRC16 is a byte-wise 256-entry table walk (slower than the word-at-a-time
  KevyHash). Routing runs once per command; if the A/B shows a visible cost
  we upgrade to slice-by-4. Not pre-optimised.
- CRC16 collisions allow slot-skew by an adversarial keyset; trust model is
  unchanged from the existing single-machine charter (no AUTH — trusted
  clients only). Recorded, not mitigated.
- Code placement: `crc16` + `key_hash_slot` go in **kevy-hash** (pure
  algorithms, stone); `slot_to_shard` lives next to `shard_of` in kevy-rt.

### D2. Port model — dual listeners

When cluster mode is ON, every shard binds **two** listeners:

| listener | port | accept behaviour | wrong-shard key |
|---|---|---|---|
| compat (existing) | `port` (SO_REUSEPORT, all shards) | kernel round-robin | forward over ring (unchanged) |
| cluster (new) | `cluster-port-base + i`, default `port + 1 + i` | deterministic per shard | `-MOVED` |

- Advertised address in CLUSTER SLOTS/SHARDS/NODES = `bind_ip:(cluster_port_base + i)`.
- Non-cluster clients keep connecting to the compat port and get the full
  current behaviour (forwarding, aggregated keyspace views) — the old bench
  angle still runs, byte-identical.
- Both reactors (epoll `Shard::run` and io_uring `run_uring`) must service
  the second listener: epoll adds one fd; uring keeps a second accept SQE in
  flight with its own user-data tag.
- Cluster mode OFF (default) = zero change: one listener, no new ports.

### D3. MOVED + cross-slot superset behaviour

- A `Conn` records which listener accepted it (`cluster_conn: bool`).
- On a cluster conn, a single-key command whose `slot_to_shard(key_hash_slot(k))
  != self.id` gets an immediate `-MOVED <slot> <ip>:<cluster_port_of_owner>`
  reply (in seq order, via the existing `immediate_reply` path) and is not
  executed. This is what keeps a cluster client's topology honest.
- Multi-key commands (MGET/MSET/SINTER/DEL/WATCH/blocking fan-out…): Redis
  rejects cross-slot with `-CROSSSLOT`; kevy is a single process with working
  cross-shard fan-out, so we keep executing them — **superset behaviour**
  (greenfield-advanced-compat: behaviour compat, not limitation compat).
  Cluster clients never send cross-slot multikey ops, so the superset is
  invisible to them. Compat note goes in README/bench REPORT.
- Keyspace-wide views (KEYS/SCAN/DBSIZE/INFO/RANDOMKEY) stay aggregated on
  every port — same superset rationale. Recorded as a compat note.
- Keyless commands (PING/CONFIG/PUBSUB/…) work on any port, unchanged.

### D4. Persistence migration — shards.meta v2

Routing change (kevyhash → slots) re-homes keys, so per-shard
`aof-{i}.aof`/`dump-{i}.rdb` written under the old routing would load
misplaced. Same problem class as embed B2 — and the **server side today has
no shards.meta at all**, so even a `--threads` change already silently strands
keys on wrong shards (existing bug; embed fixed it, server never did). This
work fixes both via one mechanism.

- `shards.meta` v2 format (shared reader/writer in **kevy-persist**):
  line 1 = shard count `n`, line 2 = routing tag (`kevyhash` | `slots`);
  missing line 2 ⇒ `kevyhash` (back-compat with embed's v1 single-number
  file). Old binaries reading a v2 file fail the `parse::<usize>()` of the
  whole string ⇒ treat as legacy ⇒ safe reshard (lossless), acceptable.
- Embed's `build_shards` switches to the shared reader/writer; its observable
  behaviour for (n, kevyhash) layouts is unchanged.
- Server bring-up: `Runtime::run` checks `(n, routing)` **before spawning
  shards**. Mismatch ⇒ centralized reshard, modeled on embed:
  load every existing snapshot+AOF into a temp `Store` (replay via
  `commands.dispatch`, same as shard restore), redistribute with
  `snapshot_each` + new routing, back up sources to `.premigration.<nanos>`,
  rewrite per-shard compacted AOFs, write meta. Then shards load in place
  as today.
- The `Value` → `Store::load_*` glue (embed's `insert_value`) moves to
  **kevy-store** as `Store::load_value(key, &Value, ttl_ms)` so both reshard
  paths share it (steel).

### D5. CLUSTER command surface + config + INFO

- Config: new `[cluster]` section in kevy-config — `enabled: bool`
  (default false), `port_base: u16` (default 0 ⇒ `port + 1`). Not
  hot-settable (routing is a startup property).
- `CLUSTER` subcommands (read-only, real implementations when enabled):
  - `SLOTS` — n entries: `[start, end, [ip, cluster_port_i, node_id_i]]`
  - `SHARDS` / `NODES` — same topology in the other two shapes; node id =
    40-hex deterministic function of (shard idx) — stable across restarts
  - `INFO` — `cluster_enabled:1`, `cluster_known_nodes:n`, `cluster_size:n`,
    state ok, 16384 assigned
  - `MYID` — the answering shard's node id (cluster conn: own shard;
    compat conn: shard that happened to accept)
  - `KEYSLOT <key>` — real `key_hash_slot` (replaces the `0` stub, also when
    cluster mode is off — it's a pure function, Redis answers it standalone
    too... actually standalone Redis answers KEYSLOT as well; match that)
  - `COUNTKEYSINSLOT <slot>` — owning shard scans its keyspace and counts
    keys whose slot matches (O(keys-of-shard), diagnostic-only; no slot
    index maintained)
- Cluster mode OFF: existing stub behaviour stays (`cluster_enabled:0`,
  KEYSLOT becomes real — see above).
- `INFO cluster` section reads the runtime config instead of the hardcoded
  block at `kevy/src/ops/mod.rs:202`.

## Execution order (each step = green tests + clippy + LOC gate)

1. kevy-hash: `crc16` + `key_hash_slot` (+ unit tests, check vector)
2. kevy-rt: routing parameterisation (`shard_of` slots path, 9 call sites,
   `Runtime::with_cluster`) — default behaviour unchanged
3. kevy-persist meta v2 + kevy-store `load_value` + embed switch +
   server-side startup reshard
4. dual listeners (epoll + uring) + `cluster_conn` flag + MOVED emission
5. kevy-config `[cluster]` + CLUSTER command surface + INFO + main.rs wiring
6. integration tests (MOVED, SLOTS shape, migration round-trip, compat
   regression)
7. bench gate on lx64: new `redis-benchmark --cluster` 8sh angle; old angles
   re-run as regression; pgrep sweep before every run

## Bench gate

- Headline: 8sh GET/SET with `--cluster` vs current ~11.2 M / ~10.3 M.
- Regression: compat-port 8sh angle must stay within noise of baseline.
- Validation: `valkey-cli CLUSTER KEYSLOT` cross-check of `key_hash_slot`
  on lx64 (valkey 9.1 available there).
