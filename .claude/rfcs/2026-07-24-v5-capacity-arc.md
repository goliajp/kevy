# RFC — v5 capacity arc: transparent tiering × virtual RDS views, fused

**Date:** 2026-07-24 · **Status:** APPROVED DESIGN (user-approved plan,
2026-07-24) — acceptance criteria are binding; T0 turns them into gates.
**Supersedes:** `2026-07-24-tiered-storage-arc.md` and
`2026-07-24-virtual-rds-views-arc.md` (kept for design history; their
standalone conclusions are folded in here, one of them — the cold
side-table — reversed by adversarial review, recorded in §7).

User mandate: acceptance criteria first; the two goals designed as ONE
arc; the mainline (especially performance) must not be materially
affected; no schedule/ROI constraints — the target is the highest
ceiling-first perf / disk / mem.

---

## 0. Why fused (three judgments)

1. **Indexes hot, rows cold.** The RDS workload's natural shape on a
   tiered store: access paths (indexes + views + stored VALUES columns)
   stay RAM-resident; row payloads (hashes) tier to disk. Queries answer
   from RAM-resident index columns — FILTER/SORT/COUNT touch zero rows —
   and only the final result page hydrates rows from the cold tier.
2. **G1 is the fusion keystone.** Generalizing the FTS doc-values
   machinery (VALUES / FILTER / SORT / DISTINCT / FACET / OFFSET) from
   text-only to Range/Unique is simultaneously the biggest RDS-views
   engine gap AND the tiering performance story: without it, every
   filtered query on a tiered table is a flood of cold reads; with it,
   cold reads happen only for the final page.
3. **Rows are hashes ⇒ Hash spill is v1-mandatory** (this revises the
   standalone tiering RFC's scalar-only v1). Hash serialization reuses
   `write_hash_payload` (kevy-persist/src/snapshot_payload.rs:24-31);
   the field-TTL sidecar (`each_hash_ttl`, kevy-store/src/snapshot.rs:43)
   must be co-serialized for losslessness. List/Set/ZSet stay hot in v1.

## 1. Core design

### D0 — Cold representation: an in-map `Value::Cold(ColdRef)` stub

`ColdRef ≈ { vlog_id: u32, offset: u64, len: u32, weight: u32,
type_tag: u8 }` ≈ 21 B — fits `Value`'s 24 B payload (the
`size_of::<Value>() ≤ 32` assert holds). `expire_at_ns` / `lru_clock`
stay on the 48 B `Entry` unchanged.

**Why the stub (and not a side-table — the reversed decision, §7):**
a side-table saves no memory (same key bytes + ColdRef + bucket
overhead) while single-handedly breaking four interaction classes:
SCAN's single-table sweep proof (a key demoted mid-sweep between two
tables is *missed* — worse than Redis's contractual duplicates), the
reaper/`expires`-counter/DBSIZE/RANDOMKEY universe, and — fatally —
the ~12 raw map probes that bypass the read funnel (`set_value_no_evict`,
the maxmemory==0 SET fast path, probes the map raw at
kevy-store/src/string_set.rs:96; DEL/RENAME/FLUSHALL/`load_*` likewise),
each of which becomes a **shadow-resurrection bug**: SET-then-DEL on a
cold key resurrects the stale cold value; SET NX wrongly succeeds;
FLUSHALL leaves the cold tier alive. With the stub, all of these work
unchanged by construction: the raw probe finds the stub (overwrite drops
the ColdRef and credits vlog dead-bytes; NX/XX read existence from it),
DEL counts it, RENAME moves it without reading the value, FLUSHALL is
`map.clear()` + reset vlog, the reaper reads `Entry.expire_at_ns` as
today, SCAN/KEYS/RANDOMKEY/DBSIZE see one table.

**Reconciling the earlier "no new Value variant" ruling** (the
standalone RFC ruled a variant out citing ~196 `Value::` match sites):
the resolution is a **two-stage funnel**. Stage 1 — existence + type
answered from the stub (zero I/O; a WRONGTYPE refusal never pays a
pread). Stage 2 — materialize (promote or peek) only when the
accessor's type matches. `Value::Cold` is resolved inside the funnel —
`live_entry`/`live_entry_mut` (accounting.rs:171,204) plus the typed
helpers' missing/type arms (`hash_value_for_set` hash.rs:376,
`list_value_for_push` list.rs:123, `zset_mut` zset.rs:199,
`set_value_mut` set.rs:87) — so the ~196 downstream match sites **never
see Cold by construction**. Exhaustive matches that do get a compile
error are a forced-review feature, not a cost. Hot-path cost: one
never-taken match arm on the hot-value path (the `maxmemory == 0`
precedent class) — gated by A1.

Without the funnel hook, today's code has a concrete data-loss shape:
`hset_one`'s missing-key arm (hash.rs:376-377) would silently create an
empty hash **shadowing the spilled fields**. The typed-helper arms are
where transparency is won; the B9 suite is the net for any missed one.

**Dedicated primitives `demote_in_place` / `promote_in_place`** — NOT
`insert_entry`/`remove_entry`, which would corrupt silently:
`insert_entry` unconditionally clears hash field-TTLs (accounting.rs:25)
and captures a `new` keyspace event; both adjust the `expires` counter.
The in-place primitives swap `entry.value` ↔ vlog record, re-stamp
`weight`, apply the `used_memory` delta, and touch nothing else — no
hfttl clear (field TTLs stay RAM-resident for cold hashes; promotion
purges fields that expired while cold), no notify capture, no `expires`
delta, no WATCH bump, `lru_clock` preserved (LFU history survives a
demotion round-trip). Demotion emits **zero** feed/notify events — it is
not a write; `note_evicted` (notify.rs:62-66) is NOT reused (clients
treat `evicted` as key removal), `evictions_total` is not incremented; a
new `demotions_total` is.

### D1 — The vlog is disposable; persistence streams from it

The AOF remains the sole durable truth; the vlog is a per-boot spill
area (deleted on open, rebuilt during replay). Zero new crash-safety
surface — the durability-trust arc (t5.5) is untouched (A4).

**But** every persistence producer iterates the map and reads values
(`snapshot_each` keyspace.rs:281-290 → `rewrite_fmt.rs:46-76` /
`snapshot_write.rs:36`; COW rewrite, replication full-sync, reshard).
Unhandled, this is the arc's #1 risk, in either direction: a side-table
design silently **omits cold keys from the rewrite → the first
BGREWRITEAOF after demotion permanently loses every cold value**; a
naive stub design promotes the whole cold tier during serialization —
the exact RAM blowup tiering exists to prevent. The clause:
`collect_snapshot` clones the *stub* into the view (still O(1)/entry,
still Send) plus a refcount **pin on `Arc<VlogFile>`** (compaction must
not invalidate a live view's offsets); materialization happens in the
serializer thread — a `Cold` arm in `write_value_as_commands` /
snapshot_write that `read_at`s the record, decodes, emits, drops.
Memory bound = one value. `read_at` takes `&File` — no promotion, no
map mutation.

**Boot with dataset > RAM is a launch-blocking clause**, not an
optimization: replay drives `insert_entry` into the hot map, so without
in-replay demotion the headline claim dies at boot (OOM before tiering
ever runs). Replay checks the demote watermark every K frames and
spills inline to the fresh vlog (single-threaded, safe). The same hook
lands in reshard's redistribution loop and the replica snapshot-load
path. Gate B11.

### D2 — Demotion / promotion engineering

- **Sampling skips Cold stubs** (cold keys are not candidates; an
  all-cold sample = no-candidate) — evict.rs:146-181 gains one arm.
- **Spill is budgeted**: ≤ N records per call (e.g. 32), continuation
  on the shard tick, bytes watermark with hysteresis — a single SET
  never funds an unbounded synchronous spill storm (the unbudgeted loop
  is `MAX_EVICTIONS_PER_CALL = 1_000_000`, evict.rs:34 — 1M spills
  inline would be a multi-second reactor stall). Heavy values hand the
  frozen Arc to background IO (io_uring: a new user_data tag class —
  `IORING_OP_READ/WRITE` are already bound and file-fd-proven,
  kevy-uring/ffi.rs + ring_tests.rs:80; poller path: the
  bio/persist_worker offload precedent).
- **Promotion policy gate**: promote on the 2nd access within a window
  (or size-gated) — protects the hot tier from bulk readers
  (digest/export/full scans) thrashing it.
- **No-promote peek** (`peek_value` / `peek_hash_field`: on a Cold stub,
  pread + decode + return WITHOUT installing) is the read primitive for
  every bulk path: hydration, index backfill (index_runtime.rs:394-414 —
  otherwise IDX.CREATE on a tiered table promotes the entire domain at
  2048 rows/tick, a tier-thrash event), PREFIX.DIGEST
  (cmd_digest.rs:23-72), scope_move, exports. One pread per ROW (the
  whole hash record decodes all fields) — never one per field.
- **Embedded RwLock reality**: a promotion pread under the (1-shard
  default) write lock stalls all readers of the shard for the pread
  duration (~50-100 µs NVMe, ms-class on cloud block storage). v1:
  accept + document + a config cap on max spillable value size in
  embedded mode. v2 (designed now, built later): drop-lock → pread by
  (vlog_id, offset, len) → relock → O(1) verify via a per-shard vlog
  epoch that ColdRef is unchanged (ColdRef is Copy + Eq) → install or
  retry.
- **Zero-copy shared lane** (`kevy_get_shared`, &self): the Cold arm
  preads and returns a fresh Arc, never promotes (documented: shared-
  lane reads pay a pread until a &mut-path access promotes).

### D3 — One unified memory budget

Two accounting sources today, never summed: per-shard
`Store.used_memory` (keyspace only, obs.rs:117) and per-index
`Segment.stats.approx_bytes` (segment.rs:65-94; agg.rs:257; view.rs:340
— enforced only at build time as per-index MAXMEM → `FailedOverBudget`,
index_runtime.rs:415-421). The unified budget:

- **Demote watermark arithmetic**: target = `budget·19/20 − index_bytes
  − stub_bytes`. Indexes/views are the premium fixed layer (never
  spilled in v1); demotion pressure applies only to hot values. If the
  floor alone exceeds the budget, IDX.CREATE refuses with a named error
  (extending the FailedOverBudget discipline). Per-index MAXMEM keeps
  its exact semantics.
- **MEMORY USAGE reports the stub's actual RAM footprint** (re-stamp
  `Entry::weight` at demotion) — preserving Σ MEMORY USAGE ≈
  used_memory, the invariant the demotion trigger itself gates on. The
  original value weight lives inside ColdRef (promotion re-accounting +
  spill-biggest-first policy input).
- **Auto-detection** (none exists today, grep-verified): Linux cgroup v2
  `memory.max` → `/proc/meminfo MemAvailable`; macOS `sysctlbyname
  "hw.memsize"` via a hand-bound kevy-sys extern (the sanctioned OS
  boundary; 0-dep preserved). `budget = "4gb" | "70%" | "auto"`,
  re-probed on the shard tick (the CONFIG-SET reapply precedent,
  commands.rs:247). Both config surfaces (kevy-config TOML/CLI/env AND
  the embedded builder).
- **INFO gauges**: cold_keys, cold_bytes, vlog_size, vlog_dead_bytes,
  demotions_total, promotions_total, index_bytes.
- wasm / mem:// : tiering config cleanly rejected (no disk).

### D4 — The RDS view layer (Law 3 unamended)

- **Engine**: G1 — VALUES/FILTER/SORT/DISTINCT/FACET/OFFSET on
  Range/Unique kinds (lifting catalog.rs:343's text-only restriction).
  Same clause grammar, same ValueTest, same law shape: stored columns
  are declared at write time; the driving predicate remains the indexed
  range/EQ; still no WHERE-without-an-index. Zero-cost when undeclared
  (A5): an index without VALUES is byte-identical in memory and query
  path — the `Option<Positions>` physical-bypass pattern.
- **Declaration layer `TABLE.*`**: `TABLE.DECLARE` compiles a
  relational table (prefix, typed columns, PK, secondary indexes,
  ORDERPATH composite-sort paths = cookbook §8 mechanized into a
  derived score index) into existing IDX/VIEW primitives at declare
  time. `TABLE.VERIFY` = component fsck + column spot checks;
  TABLE.LIST/DROP; sidecar catalog (index/view lifecycle genre). The
  engine still enforces no schema (absent field = NULL, exactly
  today), evaluates nothing at query time, chooses no access path.
  Server/embedded parity guarded by extending the dispatch oracle (the
  discipline that just caught the IDX.CREATE drift).
- **Out-of-engine `kevy-sql`**: a declaration-time compiler (CREATE
  TABLE → TABLE.DECLARE; single-table CREATE VIEW … AS SELECT with
  indexed WHERE / FILTER / ORDER BY / LIMIT → compiled named views).
  Ad-hoc per-query SQL and PG/MySQL wire emulation stay refused (Law
  3's exact red line). The consumer pitch, honestly: "your PG schema's
  access paths, compiled to explicit indexes and views, with relational
  read ergonomics at kevy speed" — not a drop-in PG.
- **Hydration on cold rows** (the fused read path): `encode_hydration`
  (cmd_index_query/wire.rs:95-107) and VIEW.HYDRATE's `op_hydrate`
  (cmd_view.rs:237-275) are today N×F serial `hget`s. Fused clause:
  resolve stubs first, coalesce by (vlog_id, offset), ONE pread per
  row via the no-promote peek, decode all F fields from it; on
  io_uring submit the batch as linked READ SQEs (new tag class) and
  complete the reply asynchronously; embedded/poller = ordered read_at
  loop. A hydrated page is not an access signal — no promotion.

## 2. Acceptance criteria (binding; every criterion maps to a gate
assertion in T0 — a criterion with no assertion does not exist)

### A. Mainline zero-harm (hard gate, veto power)
- **A1** perfgate's 12 metrics with tiering compiled in but OFF: within
  the existing 0.92 tolerance ratchet; perf-record hot profile shows no
  new symbols. Structural claim: the hot-value path gains exactly one
  never-taken match arm.
- **A2** tiering ON + working set fully hot: new perfgate lines
  (`tiered_hotset_get/set`) within the same tolerance.
- **A3** every existing gate green unchanged: crashgate / availgate /
  textgate / covgate / dispatch_oracle / repligate.
- **A4** durability contract byte-identical (crashgate; the vlog is
  non-durable by design).
- **A5** G1 undeclared = zero cost: an index without VALUES is
  byte-identical in memory + query path (memgate/idxgate formula lines
  + a perfgate "Clamp #0"-genre empty-declaration line).

### B. Tiering
- **Perf**: **B1** hot GET/SET p99 unchanged at any cold:hot ratio;
  **B2** cold GET p99 ≤ 100 µs embedded / ≤ 300 µs server e2e at 10×
  data:RAM on NVMe; **B3** demotion throughput ≥ sustained write
  ingest (no unbounded RAM growth) with spill-induced reactor stall
  p99 ≤ 1 ms (budgeted + background); **B4** boot replay with
  in-replay spill ≥ 70 % of plain replay throughput.
- **Disk**: **B5** vlog space amplification ≤ 2.0× live cold bytes
  (compaction keeps live ratio ≥ 50 % under pin semantics); **B6**
  capacity ≥ 10× data:RAM demonstrated (gate), 100× stretch.
- **Mem**: **B7** declared formulas within ±20 % (memgate): hot key =
  today's cost; cold key = 48 B Entry + key bytes (stub actual);
  **B8** RSS ≤ budget × 1.05 sustained; auto-detection correct in a
  cgroup container and on bare metal.
- **Correctness**: **B9** the transparency suite (dispatch-oracle-genre
  dual run: same op sequence, tiered vs untiered, byte-identical
  replies) over the full op surface, PLUS named specials: NX/XX on
  cold keys; WRONGTYPE with zero preads; DEL/RENAME/FLUSHALL;
  EXPIRE-family; field-TTL survival across a demote/promote round
  trip; WATCH not bumped by demotion; SCAN/KEYS/RANDOMKEY/DBSIZE see
  cold keys. **B10** tiered store passes crashgate, and
  BGREWRITEAOF/snapshot on a mostly-cold store = zero loss + bounded
  RAM (the #1-risk gate). **B11** boot with dataset > budget: replay
  completes with RSS ≤ budget × 1.05 throughout. **B12**
  demote/promote emit zero feed/notify events (counter-asserted);
  demotions_total + INFO gauges present.

### C. RDS views
- **Function**: **C1** the R1-R12 conformance suite is executable
  (indexed WHERE + residual FILTER / ORDER BY single + ORDERPATH
  composite / LIMIT+OFFSET / COUNT / GROUP aggregates / unique / Via
  FK lookup / transactions (CAS + multi-row invariants) / soft delete
  / sequences); **C2** TABLE.* round-trip (DECLARE → derived
  indexes/views, TABLE.VERIFY fsck clean; dispatch oracle extended to
  TABLE.*, server/embedded byte parity); **C3** the refusal surface
  errors by name (ad-hoc SQL / query-time joins / HAVING), never
  silently.
- **Perf**: **C4** indexed point lookup p99 ≤ 1 ms @ 10M rows; **C5**
  FILTER+SORT+LIMIT 20 page p95 ≤ 5 ms @ 10M rows, 8 shards; **C6**
  **index-only proof: row-read counter = 0** for FILTER/SORT/COUNT
  queries (non-perturbing counters — the instrument-before-concluding
  discipline); **C7** write-path tax with 3 indexes + declared VALUES
  vs bare HSET ≤ 15 %.
- **Mem/Disk**: **C8** per-index-row RAM formula (incl. VALUES
  columns) within ±20 % (memgate); per-table disk formula (AOF +
  vlog share).

### D. Fused scenarios (the real acceptance)
- **D1** 50M rows (~200 B) on a 2 GB budget: indexes hot, rows cold;
  C4/C5 hold for index-only queries; cold-row hydration page (20
  rows) p95 ≤ 10 ms; **one pread per row, not per field**
  (counter-asserted).
- **D2** index-only queries on a fully-cold table: cold-read counter
  = 0.
- **D3** hydration batching measured: N cold rows = one batched
  submission (uring linked SQEs / embedded ordered read_at).
- **D4** mixed-workload isolation: hot-set serving p99 unchanged
  while cold queries and an index backfill (no-promote peek —
  IDX.CREATE must not thrash the hot tier) run concurrently.

**Gate carriers**: new `tiergate` (B group) + `tablegate` (C1-C3);
perfgate METRICS/baseline additions (A1/A2/C4/C5/C7 at 0.92);
memgate/diskgate formula extensions (B7/C8). T0 builds the gates first,
red-first (the crashgate precedent).

## 3. Trains (linear; each five-axis gated)

- **T0 gates first**: this RFC's §2 mapped criterion-by-criterion to
  gate assertions; tiergate/tablegate skeletons; transparency-suite
  harness (red).
- **T1 `kevy-vlog` stone**: append / read_at / rotate / compact + CRC +
  **pin (Arc<VlogFile> refcount, compaction-safe) + epoch** + the hash
  record format (write_hash_payload reuse + field-TTL co-serialization);
  unit + fuzz + bench. Pure lib, zero Store coupling.
- **T2 G1 engine generalization**: VALUES/FILTER/SORT/DISTINCT/FACET/
  OFFSET on Range/Unique; A5 off-proof.
- **T3 store core**: `Value::Cold` stub + two-stage funnel (live_entry
  family + typed-helper arms) + demote/promote_in_place + evict_one
  fork + sampling skip + spill budget + promotion policy gate; B9
  suite turns green; A1/A2.
- **T4 persistence streaming** (launch-blocking correctness):
  snapshot/rewrite/full-sync/reshard stream cold values from the vlog
  without promotion (view pinning); **in-replay demotion (boot >
  RAM)**; B10/B11.
- **T5 unified budget + auto-detect + INFO**: D3 accounting; B7/B8/B12.
- **T6 hydration cold-batching + no-promote peek adoption**:
  encode_hydration/op_hydrate collect-batch-encode; backfill / digest /
  scope_move on the peek; the uring file-read tag class; D1(hydration)/
  D3/D4.
- **T7 `TABLE.*` declaration layer**: compile-to-IDX/VIEW + ORDERPATH +
  VERIFY; dispatch-oracle extension; tablegate C1-C3.
- **T8 `kevy-sql` compiler** (out of engine) + the "porting a PG/MySQL
  schema" cookbook chapter.
- **T9 capacity close-out**: D1/D2/D4 envelope on a dedicated box (10×
  gate, 100× stretch); memgate/diskgate formulas; trilingual docs;
  five-axis final audit.
- **Named v2 (not in this arc)**: embedded drop-lock/pread/relock;
  fully-async cold reads; collection spill; kvrocks competitive bench;
  the G4 view-FILTER constitution round.

## 4. Interaction matrix (adversarial review, 2026-07-24 — mechanics
verified at file:line; each row: failure if unhandled → clause)

1. **SCAN/KEYS/RANDOMKEY/DBSIZE** (exec_scan.rs:30-37, kevy-map
   scan.rs:41-57, keyspace.rs:221-242) — side-table breaks the sweep
   proof → in-map stub, single table, TYPE filter answers from the
   type_tag.
2. **Snapshot/rewrite/full-sync/reshard** (keyspace.rs:281-290,
   rewrite_fmt.rs:46-76) — side-table = permanent cold-value loss on
   first rewrite; naive stub = full-tier promotion → stream from
   pinned vlog on the serializer thread, no promotion.
3. **Replication/feed** (exec_dispatch.rs:346, notify.rs:62-66) —
   reusing evict_one verbatim fires `evicted` notifications and
   inflates evictions_total → dedicated spill path, zero events,
   demotions_total.
4. **TTL/expiry/DEL** (expire.rs:95-108, accounting.rs:178-187,
   lib.rs:219-225) — side-table removes cold keys from the reaper
   universe and the expires counter → stub keeps expire_at_ns on
   Entry; expiry never needs the value; remove_entry's Cold arm
   credits vlog dead-bytes.
5. **Typed writers' missing-key arms** (hash.rs:376, list.rs:123,
   zset.rs:199, set.rs:87) — LPUSH on a cold Str creates a list
   (should be WRONGTYPE); HSET on a cold hash creates an empty shadow
   → two-stage funnel: type from stub, WRONGTYPE without a pread,
   materialize only on type match.
6. **MEMORY USAGE / TYPE** (ops/memory.rs:56-65, keyspace.rs:213-219)
   — TYPE must not read disk → ColdRef carries type_tag (3 consumers:
   TYPE, SCAN's TYPE filter, WRONGTYPE precheck); MEMORY USAGE
   reports stub footprint (preserves Σ ≈ used_memory).
7. **Zero-copy shared lane** (string.rs:61-89) — &self cannot promote
   → Cold arm preads, returns a fresh Arc, never promotes.
8. **Embedded RwLock** — pread under the 1-shard write lock stalls
   all readers → v1 accept + document + spillable-size cap; v2
   drop-lock/pread/relock with epoch verify.
9. **Index maintenance/backfill** (index_runtime.rs:288-333, 394-414)
   — backfill would promote the whole domain → no-promote peek, one
   pread per row; `exists` is stage-1 metadata-only.
10. **Raw map probes** (string_set.rs:96, keyspace.rs:23-32, 147-181,
    259-264) — the side-table's shadow-resurrection class → dissolved
    by the stub.
11. **insert/remove_entry side effects** (accounting.rs:25, 46-52) —
    reuse would wipe hash field-TTLs, fire spurious events, drift
    `expires` → demote/promote_in_place primitives.
12. **Eviction sampling/loop** (evict.rs:95-181) — stubs as victims
    degenerate the loop; unbudgeted spill = reactor stall → skip
    Cold in sampling; budget + tick continuation + hysteresis.
13. **Hydration** (wire.rs:95-107, cmd_view.rs:237-275) — N×F preads
    + promotion churn → one pread per row, coalesced, batched,
    no promotion.
14. **Boot replay > RAM** (replay.rs, keyspace.rs:292-483) — OOM at
    boot falsifies the headline → in-replay demotion (launch-
    blocking); same for reshard + replica load.
15. **PREFIX.DIGEST / scope_move / export** (cmd_digest.rs:23-72) —
    full-tier promotion feedback loop → peek + promotion policy gate.
16. **Index bytes vs budget** (index_runtime.rs:415-421) — demotion
    can only reclaim values; segments are a floor → watermark
    arithmetic subtracts index_bytes + stub_bytes; IDX.CREATE refused
    when the floor exceeds budget.

**Top-5 risks**: (1) cold values lost from rewrite/snapshot (§D1
clause); (2) funnel-bypass shadow state (dissolved by the stub);
(3) boot > RAM (in-replay demotion, launch-blocking); (4) demote/
promote side-effect leakage (in-place primitives); (5) reactor/lock
stalls on synchronous IO (budget + background + batch clauses).

## 5. Law/charter check

Law 1: every Redis op works on cold keys; cold keys are ordinary keys
to SCAN/TYPE/TTL; no new frame types; new verbs live in TABLE.* dotted
namespace. Law 2: TABLE compiles to declared, explicitly-named access
paths. Law 3: unamended — no query language in the engine, no planner,
no query-time joins (kevy-sql is out-of-engine and declaration-time).
0-dep: std::fs read_at + hand-bound sysctl/cgroup probes in kevy-sys.
Durability (t5.5): AOF contract untouched; vlog explicitly non-durable.

## 6. Honest consumer framing

Not a drop-in PG/MySQL: no ad-hoc runtime SQL, no query-time joins
beyond Via lookup, no HAVING/subqueries/window functions, no
engine-enforced constraints (CHECK = atomic-block recipe; uniqueness =
verify-not-enforce). And tiering v1: scalar + hash values spill;
lists/sets/zsets/streams stay hot; embedded cold reads hold the shard
lock (documented, capped, v2-designed).

## 7. Design history (recorded, not hidden)

The standalone tiering RFC (superseded) proposed a cold **side-table**
and ruled out a `Value` variant, citing ~196 match sites and the 32 B
assert. Adversarial review (16-interaction matrix, this document §4)
reversed it: the side-table saves no memory and creates four
independent corruption/semantic classes (SCAN guarantee, reaper
blindness, shadow resurrection at ~12 raw-probe sites, FLUSHALL leak),
while the variant's blast radius dissolves under the two-stage funnel
(downstream matches never see Cold) and ColdRef fits the existing
32 B envelope. The 196-site argument was answered, not overruled: the
variant is confined to the funnel by construction, and exhaustive-match
compile errors are the review mechanism. This reversal is the reason
the fused design round was run adversarially before any code.
