# idx / page query pipeline decomposition — kevy 4 vs PostgreSQL 18

> R3 Phase A product (RFC: `.claude/rfcs/2026-08-04-v5-r3-query-path-decomp.md`).
> READ-ONLY decomposition; no attack has been implemented. Every µs figure in
> this document is an estimate from the cost table below unless marked
> **[measured]**; every claim marked **[RUNTIME-VERIFY]** requires a counter or
> probe before Phase B may treat it as ground truth (methodology §2 hard rule:
> source-only reading is necessary, NOT sufficient).

## Measured baseline (Gate 1, lx64 16-core, 2M rows × 440 B in cache, median of 3)

Source: `bench/R3-GATE1-2026-08-04.log`, harness `bench/pgcompare.py`.

| shape | engine | p50 | p99 | p99/p50 |
|---|---|---:|---:|---:|
| idx (`sku = k LIMIT 20`) | PG18 stock | 124 µs | 164 µs | 1.32× |
| idx | kevy none | 134 µs | 384 µs | **2.87×** |
| page (`dept = d AND ts BETWEEN … ORDER BY ts LIMIT 20`) | PG18 stock | 112 µs | 147 µs | 1.31× |
| page | kevy none | 159 µs | 231 µs | 1.45× |

Two distinct questions, answered separately:

* **Q1 (p50 path)** — idx nearly tied (134 vs 124), page 1.42× behind.
  Stage-by-stage accounting below (S01–S19).
* **Q2 (p99 tail)** — kevy idx tail is +250 µs over its own p50 while PG's is
  +40 µs. Tail mechanisms enumerated in "Cross-cutting overhead" (O1–O10),
  each with file:line, expected frequency, magnitude, and a verification
  probe.

### Wire shapes (exactly what the harness sends — `bench/pgcompare.py:329-340`)

* kevy idx: `IDX.QUERY t.sku EQ <k> LIMIT 20` — **no FIELDS clause**, so zero
  hydration store reads; the reply is 20 × (row key, sku value).
* kevy page: `IDX.QUERY t.by_dept_ts WHERE dept EQ <d> RANGE ts <lo> <lo+2000> LIMIT 20`
  — composite ORDERPATH, no FIELDS, no FILTER/SORT/OFFSET ⇒ takes the
  **plain scalar path** (`run_scalar_query`), not the claused path.
* PG idx: `SELECT id, name FROM t WHERE sku = %s LIMIT 20`;
  PG page: `SELECT id, name, ts FROM t WHERE dept = %s AND ts BETWEEN %s AND %s ORDER BY ts LIMIT 20`.
  psycopg3 `%s` = extended protocol; `prepare_threshold` defaults to 5, so
  after 5 executions both statements run as **named prepared statements**:
  parse+plan is amortized, each sample pays Bind/Execute/Sync only.
  (Fairness note: PG returns 2–3 hydrated columns per row; kevy returns
  key + driving value. PG does strictly more row work per hit — S14.)
* Server launch: `bench/pgcompare.sh:115` starts kevy with no `--threads`
  ⇒ `available_parallelism()` (`crates/kevy/src/main.rs:76`) ⇒
  **16 shards** on lx64 (`bench/PGCOMPARE-2026-07-26.md:19`, "16 cores").
  Every `Route::Extension` query fans out to all 16.

### Selectivity (drives hit counts per stage)

* idx: ~20 rows per sku, scattered uniformly ⇒ ~1.25 hits/shard, Σ ≈ 20.
* page: rows with `dept=d` are `i ≡ dept (mod 8)`; ts window of 2000
  consecutive i values ⇒ ~250 matching rows, ~15.6 hits/shard, **Σ ≈ 250
  hits shipped to origin, truncated to 20 after the k-way merge** (12.5×
  over-fetch — each shard must return up to LIMIT because all 20 global
  winners could sit on one shard). PG reads **exactly 20** index entries
  (LIMIT stops the scan; index order = output order).
  **[RUNTIME-VERIFY]**: `IDX.COUNT t.by_dept_ts WHERE dept EQ eng RANGE ts <lo> <hi>`
  on the loaded box should answer ≈ 250; a debug counter on chunk hit
  totals should show idx Σ≈20 / page Σ≈250.

### Atomic-op cost table (methodology §4, Apple-M baseline; lx64 x86 syscalls +10-30 %)

| op | cost | op | cost |
|---|---|---|---|
| L1 hit | 1 ns | heap alloc (small) | 30–50 ns |
| L3/DRAM miss | 50–100 ns | BTree descent (~125k entries) | 300–1000 ns |
| atomic load | 1–2 ns | HashMap get (FxHash-class) | 30–50 ns |
| atomic CAS/fetch_or | 5–10 ns | pipe/eventfd `write()` | 1.0–1.5 µs |
| SPSC ring push | 20–50 ns | thread wake from blocked wait | 3–30 µs (p50), 50–300 µs (p99) |
| RwLock read uncontended | 10–50 ns | TCP loopback send/recv <8 KB | 1–3 µs kernel-side |
| `write()`/`sendall` syscall | 1–2 µs | PG shared-buffer hash lookup | 300–500 ns |

---

## Stages

Legend: each stage lists the kevy path (file:line, entry→exit), the PG18 path
(`/tmp/pg18-src` = REL_18_STABLE), enumerated atomic ops, µs estimate
(idx / page where they differ), Δ + cause, attack candidate if any.
Per-shard stages marked ×16 run on all shards **in parallel**; only the
slowest instance is on the critical path, but the origin-side per-target
loops (S05, S07, S16) are **serial** on the origin thread.

### S01 — client request encode + send

* kevy: `pgcompare.py:243-273` `enc()` + `sock.sendall` — pure python bulk
  framing (6 args idx / 11 args page), 1 syscall.
* PG: psycopg3 `cursor.execute` on a prepared statement — C-accelerated
  Bind/Execute/Sync packet build, 1 syscall.
* Ops: kevy ~10 python-level string ops + 1 `sendall`; PG ~1 C encode + 1 send.
* µs: kevy **6**; PG **10** (psycopg's python→C call layers cost more than
  `b"".join`; both include the ~1.5 µs send syscall).
* Δ: −4 in kevy's favor. Client cost lands on both sides by harness design.

### S02 — kernel loopback + server-side wakeup

* kevy: TCP loopback delivery → multishot-recv CQE
  (`crates/kevy-rt/src/uring_reactor.rs` recv arm; conn owned by ONE origin
  shard). If the origin shard is inside its 256-iteration spin window
  (`URING_SPIN_LIMIT`, `uring_reactor.rs:44`) the CQE is reaped in ≤ ~1 µs;
  if it already parked (`uring_park.rs:24-60`, blocking
  `submit_and_wait(1)`), add a thread wake (3–30 µs); if it armed the
  **nap** (`uring_reactor.rs:448-450`) the CQE waits out the remainder of a
  deaf 200 µs sleep — that is tail mechanism **O1**, not p50.
* PG: backend blocked in `secure_read` → socket wake, 1 process, same cost
  class.
* µs: kevy **5** (p50: caught in spin or cheap park-wake); PG **5**.
* Δ: 0 at p50. At p99 this stage is kevy's largest term (O1/O2).

### S03 — request parse

* kevy: `kevy_resp::parse_command_borrowed` via `Shard::dispatch_batch`
  (`crates/kevy-rt/src/inbox.rs:32-55`) — zero-copy borrowed argv.
* PG: `exec_bind_message` (`src/backend/tcop/postgres.c:1625`) — read
  portal/statement names, 1–3 param datums, text→Datum coercion.
* Ops: kevy ~6–11 slice scans; PG StringInfo walk + `OidInputFunctionCall`
  per param.
* µs: kevy **0.5**; PG **4**.
* Δ: −3.5 kevy. (PG's extended-protocol bookkeeping is real per-query work.)

### S04 — route / plan lookup

* kevy: `kevy_resolve` → `route_for_verb` `IDX.QUERY … => Route::Extension`
  (`crates/kevy/src/cmd_resolve.rs:184`) — one match arm.
* PG: `GetCachedPlan` (`src/backend/utils/cache/plancache.c:1280`) on the
  named prepared statement — revalidation (dependency + ACL check), generic
  plan reuse (chosen after 5 execs).
* µs: kevy **0.3**; PG **3**.
* Δ: −2.7 kevy.

### S05 — execution setup / target build

* kevy: `start_multi` → `build_multi_targets` `Route::Extension` arm
  (`crates/kevy-rt/src/exec_build.rs:101-107`): materialize owned argv
  (`args[i].to_vec()` × 6/11), then **`argv.clone()` per target × 16** plus
  one copy in the `Agg::ExtensionGather` — ≈ 17 deep clones ≈ 17 × (1 outer
  + 6/11 inner) ≈ 120/200 small allocs; then `push_pending_slot`.
* PG: `PortalStart` (`src/backend/tcop/pquery.c:434`) + `ExecutorStart`
  (`src/backend/executor/execMain.c:122`): snapshot (`GetSnapshotData`),
  `LockRelationOid` ×2 (table + index), `ExecInitNode` for Limit+IndexScan,
  `index_beginscan`, scan-key setup. Heavier than kevy's clone loop but
  amortizes nothing (per execute).
* µs: kevy **5 / 7**; PG **15**.
* Δ: −8..−10 kevy at this stage — kevy's win here is what keeps p50 close
  despite S07/S08.
* Attack (A4): share `Arc<[Vec<u8>]>` across the 16 `Op::Extension` targets
  instead of 16 deep clones — see Top-N.

### S06 — origin's own shard executes its slice (inline)

* kevy: `dispatch_targets` runs `shard == self.id` inline
  (`crates/kevy-rt/src/exec.rs:344-349`) → `exec_op` `Op::Extension` arm
  (`crates/kevy-rt/src/exec_op.rs:183-186`) → `extension_op`
  (`crates/kevy/src/cmd_index_query.rs:84-130`) → `op_query` → this is one
  instance of S09–S15; serial on the origin before the wake flush.
* PG: n/a (single process — all execution is S11–S15).
* µs: kevy **3 / 5.5** (breakdown under S09–S15).
* Δ: pure kevy-side serial add.

### S07 — cross-shard dispatch: ring pushes + wake syscalls

* kevy: 15 × `send_to(shard, Inbound::Request{origin, conn, seq, op})`
  (`crates/kevy-rt/src/exec.rs:350-360` → `shard_flush.rs:78-100`): SPSC
  ring push + `inbound_dirty` `fetch_or` + `pending_wakes` bit; then ONE
  `flush_wakes` at the end of the reactor iteration
  (`uring_reactor.rs:302` → `shard_flush.rs:37-60`): SeqCst fence + per
  parked peer **one `write()` on its self-pipe waker**
  (`crates/kevy-sys/src/waker.rs:43`). Under this sequential single-client
  workload all 15 peers are parked between queries (their inter-arrival
  ≈ full query period ≫ 256-spin window) ⇒ **15 serialized wake syscalls
  ≈ 15 × 1.2–1.5 µs ≈ 18–22 µs on the origin's critical path**.
  **[RUNTIME-VERIFY]**: `strace -c -e trace=write -p <origin tid>` during
  the idx phase should show ≈ 15 writes/query; or a `wakes_sent` counter in
  `flush_wakes_slow`.
* PG: none — no fan-out exists.
* µs: kevy **20** (15 × 1.3 syscall + 15 × 0.05 ring push + fence); PG **0**.
* Δ: +20 — the single largest structural p50 term kevy pays and PG does not.
* Attack (A2): batch the 15 wakes into one ring submit via
  `IORING_OP_MSG_RING` — see Top-N.

### S08 — peer wake latency (parallel; max of 15 on the critical path)

* kevy: each peer sleeps in `submit_and_wait(1)`
  (`crates/kevy-rt/src/uring_park.rs:56`); the waker-pipe CQE completes it;
  kernel wake + CFS run-queue latency per peer. The stage cost is
  **max over 15 draws**, and the 15 wakes are themselves issued serially
  (S07), so peer 15's clock starts ~20 µs after peer 1's.
* PG: none.
* µs: kevy **25** (p50 of max-of-15 given per-draw p50 ≈ 5–15 µs); PG **0**.
  **[RUNTIME-VERIFY]**: `perf sched record`/`sched:sched_wakeup` →
  wake-to-run histogram for the 16 kevy threads during the idx phase; or
  stamp `Inbound::Request` with a TSC at send and diff at `exec_op` entry.
* Δ: +25. Also the second tail mechanism (O2): the p99 of a max-of-15 is
  far worse than of one draw — see Q2.

### S09 — per-shard query parse (×16)

* kevy: `extension_op` verb sniff (~8 `eq_ignore_ascii_case` probes,
  `cmd_index_query.rs:84-129`) + `Query::parse`
  (`crates/kevy/src/cmd_index_query/args_scalar.rs:67-92`; page adds
  `kevy_index::parse_where`, `composite.rs:179-207`) — clones name + shape
  literals (3–6 small allocs) **on every shard**: the same argv is parsed
  16 times, then a 17th time in the reduce (S17).
* PG: parse happened once at PREPARE, five samples ago. Zero.
* µs: kevy **0.7 / 1.0** per shard (parallel); PG **0**.
* Δ: structural: kevy re-derives the query per shard instead of shipping a
  compiled form. Small in µs at 16 shards but scales with fan-out width.

### S10 — catalog generation check + segment lookup (×16)

* kevy: `with_ready_segment` (`crates/kevy/src/index_runtime.rs:172-190`):
  `RefCell::borrow_mut` + `refresh` = one Acquire load of `index_gen`
  (`crates/kevy/src/state/catalogs.rs:153-155`, generation unchanged ⇒
  early return) + linear `find` over 2 `ShardIndex` entries by name bytes.
* PG: relcache/plancache pinned at bind; `LockRelationOid` already counted
  in S05.
* µs: kevy **0.15**; PG **0**.
* Δ: negligible. (The catalog RwLock is NOT taken on the shard hot path —
  only the atomic gen. Confirmed `catalogs.rs:26-30` comment and code.)

### S11 — bounds computation (×16)

* kevy idx: `Query::bounds_for` → `parse_literal_bound(I64)` — one int
  parse, value cloned for (min,max). kevy page: `composite_bounds`
  (`crates/kevy-index/src/composite.rs:278-318`) — encode dept (str-framed,
  6 B) + ts min/max (`order_key` 8 B) into two ~14 B byte strings, ~6
  allocs.
* PG: `_bt_preprocess_keys` inside `_bt_first`
  (`src/backend/access/nbtree/nbtsearch.c:907`) — scan-key normalize, no
  alloc-heavy work.
* µs: kevy **0.2 / 0.5** per shard; PG **1**.
* Δ: ~0.

### S12 — tree descent (×16 parallel vs once)

* kevy: `Segment::range` (`crates/kevy-index/src/segment.rs:222-252`) on
  `BTreeSet<(IndexValue, Vec<u8>)>` holding ~125k entries/shard
  (2M/16): `tree.range((Included((min.clone(), Vec::new())), Unbounded))` —
  bound construction clones min (+1 alloc) — descent ~4–5 node levels,
  ~4–8 cache misses. idx compares `I64`; page compares 14-B `Str`
  (memcmp).
* PG: `_bt_first` → `_bt_search` (`nbtsearch.c:107`) on a 2M-entry btree:
  root(cached)+internal+leaf = 3 levels, each `_bt_binsrch`
  (`nbtsearch.c:348`) over a 8 KB page + buffer-hash lookup + content
  lock. ONE descent total.
* µs: kevy **1.0 / 1.5** per shard (parallel ⇒ ~1.5 on critical path);
  PG **3** (buffer manager overhead per level beats kevy's in-process
  BTreeSet — kevy's descent is genuinely cheaper, it just runs 16 of them).
* Δ: −1.5 per instance for kevy; ×16 in aggregate CPU but parallel in
  latency.

### S13 — leaf walk + hit collection

* kevy: same `Segment::range` loop: per hit `out.push((k.clone(),
  v.clone()))` — **2 heap allocs per hit** (key ~11 B, value 8 B/14 B).
  idx: ~1.25 hits/shard; page: ~15.6 hits/shard ⇒ ~31 allocs/shard.
* PG: `_bt_readpage` (`nbtsearch.c:1649`) batches all matching TIDs on the
  leaf into `so->currPos` in one page pass — **no per-hit alloc**, ~20 TID
  copies; page shape: entries are contiguous (dept,ts) so one leaf page
  covers all 20.
* µs: kevy **0.15 / 2.5** per shard; PG **2** (idx: ~20 scattered but
  same-leaf sku entries; page: 20 contiguous).
* Δ: kevy's per-hit clone tax shows on page (16 shards × 15.6 hits × 2
  allocs = 500 allocs total vs PG's 0).

### S14 — row fetch / hydration

* kevy: **zero** — no FIELDS clause ⇒ `peek_hydration` early-returns
  (`crates/kevy/src/cmd_index_query/wire.rs:107-117`, `fields.is_empty()`);
  `encode_hits_chunk` emits `[fcount=0]` per hit.
* PG: 20 × `index_getnext_slot` → `index_fetch_heap`
  (`src/backend/access/index/indexam.c:679,720` →
  `heapam_handler.c:115` → `heap_hot_search_buffer` `heapam.c:1718`):
  buffer-hash lookup + pin + content lock + HOT-chain walk + visibility
  check, then `slot_getsomeattrs` deform of 2–3 columns from a 440 B
  tuple. idx: 20 **random** heap pages; page: ts correlates perfectly with
  load order ⇒ the 20 tuples sit on **2–3 adjacent heap pages** (buffer
  lookups amortize) — this is why PG page (112) beats PG idx (124) while
  kevy page is slower than kevy idx.
* µs: kevy **0**; PG **22 (idx) / 16 (page)**.
* Δ: −22/−16 kevy — kevy's biggest single p50 win, and it is a *work*
  difference, not efficiency: PG returns hydrated columns, kevy returns
  key+value only. Any future FIELDS-carrying benchmark shape erases this
  stage's advantage.

### S15 — result encode, server side (×16 vs once)

* kevy: `encode_hits_chunk` (`crates/kevy/src/cmd_index_query/query.rs:203-221`):
  status byte + u32 count + per hit (u32 klen, key, tagged value, 0-field
  hydration byte) ⇒ idx ~50 B, page ~560 B per shard chunk; a few Vec
  growth reallocs.
* PG: `printtup` (`src/backend/access/common/printtup.c:304`) × 20 —
  DataRow message per row (2–3 columns, out-function per column:
  `int8out`/`textout`), into the libpq send buffer.
* µs: kevy **0.3 / 1.2** per shard (parallel); PG **12** (20 × ~0.6).
* Δ: −10 kevy: binary LE chunk vs per-column text out-functions.

### S16 — response transport + origin fold (kevy only)

* kevy: each peer `send_to(origin, Inbound::Response{part})`
  (`crates/kevy-rt/src/inbox.rs:230-234`) — ring push; origin is spinning
  (`xshard_inflight > 0` holds it in the spin rung,
  `uring_reactor.rs:442-445`) so peers **skip the wake syscall**
  (`parked[origin] == false` in `flush_wakes_slow`, `shard_flush.rs:56`).
  Origin `uring_drain_inbound` → `drain_inbound_core_slow`
  (`inbox.rs:196-359`): per response `xshard_inflight -= 1` + `fold`
  (`crates/kevy-rt/src/exec_fold.rs:53-55` pushes chunk into
  `Agg::ExtensionGather.chunks`) + `mark_pending_write_dirty`.
* PG: none.
* µs: kevy **6** (15 × ~0.4, partially overlapped with stragglers); PG **0**.
* Δ: +6.

### S17 — reduce: decode + merge + truncate (kevy) / nothing (PG)

* kevy: last fold triggers `extension_reduce`
  (`exec_fold.rs:188-203` → `crates/kevy/src/cmd_index_reduce.rs:28-76`):
  `triage_status` scans 16 chunk status bytes; `observe_hit` = one usage
  RwLock read + 2 relaxed stores (`cmd_index_reduce/advise.rs:126-142`,
  `catalogs.rs:69-71`); `reduce_query`
  (`crates/kevy/src/cmd_index_reduce/query.rs:141-183`): **re-parses the
  argv (17th parse)**, then decodes EVERY hit of EVERY chunk into owned
  `(IndexValue, Vec<u8>, Hydrated)` — idx: 20 hits ≈ 40 allocs; page:
  **~250 hits ≈ 500 allocs + memcpys** — then `sort_by` on
  `(value, key)` (idx: 20 elements; page: 250 × ~8 cmp levels of 14-B
  memcmp + 40-B element swaps), `truncate(20)`, throwing away 230
  decoded-and-allocated hits.
* PG: none — index order IS output order; LIMIT already stopped the scan
  at 20.
* µs: kevy **4 (idx) / 24 (page)**; PG **0**.
* Δ: +4/+24 — **the whole kevy page-vs-idx p50 gap (159−134 = 25 µs) is
  this stage plus S13's extra clones.** Attack A3.

### S18 — reply encode + write

* kevy: cursor hex + RESP array of 2N bulks (20 keys + 20 value reprs,
  ~1.3 KB) (`reduce/query.rs:167-182`), `fill_extension_slot` →
  `drain_front` → write SQE arm + kernel send.
* PG: CommandComplete + ReadyForQuery after the DataRows (already sent
  per-row in S15); one `send()` flush.
* µs: kevy **4**; PG **3**.
* Δ: ~0.

### S19 — client receive + reply parse

* kevy: python `K.reply()` (`pgcompare.py:258-268`) — recursive: 1 array
  header + 42 bulk reads via buffered `readline()`/`read()` ⇒ ~85 python
  method calls.
* PG: psycopg3 C row parser + `fetchall()` of 20 tuples.
* µs: kevy **28**; PG **14**.
* Δ: +14 **against** kevy — the harness's own python parser is ~2× slower
  on kevy's 42-element flat array than psycopg's C loop on 20 DataRows.
  Additive on both sides by design, but NOT symmetric in magnitude.
  **[RUNTIME-VERIFY]**: time `K.reply()` alone against a canned 42-bulk
  frame (~µs-level python microbench) before attributing this stage's
  share.

### S20 — client bookkeeping (both sides)

* `perf_counter_ns` ×2, rng, list append: **2 µs** each side. Δ 0.

---

## Cross-cutting overhead — Q2: where can kevy intermittently lose >100 µs?

Every mechanism on the kevy server that can insert latency onto a query's
critical path without being on the p50 path. PG comparison point: a PG
backend is ONE process that never hands the query to another thread — its
only tail sources are buffer misses and kernel scheduling of itself
(measured p99/p50 = 1.31×).

### O1 — origin shard's batch-gated NAP: a deaf 200 µs sleep armed by the query's own responses ★ leading hypothesis

* Where: `crates/kevy-rt/src/uring_reactor.rs:448-450` — idle ladder rung 2:
  `if !napped && last_inbound_batch >= NAP_BATCH_MIN { thread::sleep(200 µs) }`
  with `NAP_US = 200` (`:48`), `NAP_BATCH_MIN = 4` (`:52`);
  `last_inbound_batch = did_inbound` (`:469-471`) where `did` counts **every
  inbound message including `Inbound::Response`**
  (`crates/kevy-rt/src/inbox.rs:215,226-249,275-296`).
* Mechanism: an idx/page query makes the origin drain ~15 `Response`
  messages — one drain batch of ≥ 4 ⇒ the nap arms. After the reply is
  written, the origin spins 256 empty iterations (~25–50 µs); if the
  client's next request has not arrived yet (python turnaround ≈ 25–45 µs
  — a race the client loses whenever it is a little slow), the origin goes
  **deaf for up to 200 µs**; the next request's recv CQE waits out the
  remainder. The nap was built for the 8-shard OWNER shape (drains
  `RequestBatch`es) — counting the *origin's own responses* as an
  "aggregation batch" is the mis-fire.
* Expected frequency: the fraction of samples where client turnaround
  exceeds the spin window — order 1–20 % of queries; magnitude: up to
  200 µs, mean ~100 µs when hit.
* Fit to data: idx p99 − p50 = 250 µs ≈ nap (≤200) + wake tail (O2);
  idx mean − p50 = 28 µs ≈ 15 % of queries eating ~180 µs. **page's smaller
  tail (+72 µs) fits too**: page peers do 5–10× more work, response
  arrivals at the origin spread out, the LAST drain batch is small (<4)
  more often ⇒ nap arms less. pk never fans out (batch = 1) ⇒ no nap ⇒
  p99/p50 = 2.9× only appears on fan-out verbs. AOF mode irrelevant —
  matches idx p99 ≈ 370–390 across none/everysec/always/tiered
  **[measured]**.
* **[RUNTIME-VERIFY]** (any one is decisive):
  1. counter: `naps_entered` per shard (one `u64` beside `napped`), dumped
     via INFO/eprintln after the lat phase — predicts ≈ 1–20 % of idx
     samples, ~0 for pk;
  2. A/B: rebuild with `NAP_BATCH_MIN = usize::MAX` (1-line diag) → idx
     p99 should collapse toward ~250 µs while 8-shard legacy throughput
     bench (`legacy_8sh_set`) guards the regression the nap exists for;
  3. zero-code: `--threads 1` run (no fan-out, batch always 1) — see O2's
     probe, disambiguates O1 vs O2 jointly with (1).

### O2 — max-of-15 park-wake scheduler latency

* Where: `crates/kevy-rt/src/uring_park.rs:24-60` (peers block in
  `submit_and_wait(1)`); wake = origin's serialized pipe writes
  (`shard_flush.rs:50-58`, `kevy-sys/src/waker.rs:43`).
* Mechanism: query completion requires ALL 16 shards to answer ⇒ latency
  includes the **max of 15 wake-to-run draws**. If a single draw has
  P(>100 µs) = p (C-state exit, CFS delay, sibling load), the per-query
  tail probability is 1−(1−p)^15 ≈ 15p. pk pays 1 draw (its p99−p50 =
  +44 µs **[measured]** bounds the single-draw tail); idx pays the max of
  15.
* Expected frequency: every query; magnitude p50 ~5–25 µs, p99 50–300 µs.
* **[RUNTIME-VERIFY]**: `--threads` sweep {1, 4, 8, 16} on the same box,
  same dataset: p99 should fall monotonically with fan-out width if
  O1+O2 dominate (at `--threads 1` both vanish: predict idx p50 ≈
  70–90 µs, p99/p50 ≈ 1.3× — PG-shaped); plus `perf sched record` on the
  idx phase for the wake-to-run histogram.

### O3 — the 15 serialized wake `write()` syscalls themselves

* Where: `shard_flush.rs:50-58` loop.
* Constant ~20 µs at p50 (counted in S07), but variance couples to O2:
  peers that were still spinning skip the syscall, so the count per query
  fluctuates 0–15 with the previous query's timing.
* **[RUNTIME-VERIFY]**: `strace -c` write counts per lat phase (S07 probe).

### O4 — shard tick maintenance sharing the shard thread

* Where: `uring_reactor.rs:325-374` — every 256 iters or on park wake:
  `tick_blocked_timeouts`, `tick_xshard_timeouts`, `tick_repl_waiters`;
  at 100 ms cadence (`shard_tick_interval_ms` default): `on_shard_tick` →
  index `on_tick` (`crates/kevy/src/index_runtime.rs:88-120`: gen check +
  2 ready segments, backfill/window both no-op here) + TTL reaper (no
  TTLs in this dataset) + `tick_persist` (`shard_tick.rs:138-150`, AOF off)
  + conn gauge.
* Expected magnitude with THIS workload: ~1–10 µs per fire — **cannot
  explain a 250 µs tail**; at 10 Hz × 16 shards the collision probability
  with a 130 µs query is ~2 % but the inserted delay is small.
* **[RUNTIME-VERIFY]**: wrap the `on_shard_tick` branch in a TSC delta
  counter (max observed per phase); expect max < 50 µs.

### O5 — AOF activity

* Excluded by data: `none` mode has no AOF and shows the same idx p99
  (384 vs 369/384/373 across modes) **[measured]**.

### O6 — allocation spikes / hash resize

* The read phase performs no keyspace writes (writes are a separate lat
  bucket) ⇒ no store-side rehash. Query-path allocs are ~100s of
  small blocks (S05/S13/S17) — kevy-alloc small-class churn, no global
  lock. Not a >100 µs mechanism. **[RUNTIME-VERIFY]** only if O1–O2 fail
  to close the tail: alloc-stall counter in kevy-alloc.

### O7 — catalog / advise locks

* Served-query path: `usage` RwLock read + relaxed stores at the origin
  reduce (`advise.rs:131-142`), `advise` Mutex only on REFUSED queries
  (`cmd_index_reduce.rs:35-38`) — none here. Catalog installs (write lock)
  only at DECLARE time. Not a tail source in this phase.

### O8 — client-side python jitter (GC, allocator)

* Lands on both engines; PG's 1.31× p99/p50 bounds its magnitude (~+40 µs).
  It cannot explain the kevy−PG p99 DIFFERENCE but inflates both absolute
  p99s. **[RUNTIME-VERIFY]**: `gc.disable()` variant run if 1-in-100
  python pauses are suspected of stacking with O1.

### O9 — multishot-recv rearm / provided-buffer exhaustion

* `uring_reactor.rs` recv path: buffer-ring refill and occasional rearm
  CQE; single 50-byte-request client cannot exhaust a per-shard buffer
  ring. Negligible here.

### O10 — origin parked (not napped) when the request lands

* Ordinary park-wake on the origin adds one draw (~3–30 µs) to S02 —
  contributes mid-tail spread, subsumed by O2's single-draw statistics
  (pk shows it: p99 67 vs p50 23 **[measured]**).

---

## Budget validation (±20 % hard bar, against Gate-1 p50 medians)

kevy idx (target 134 µs): S01 6 + S02 5 + S03 0.5 + S04 0.3 + S05 5 +
S06 3 + S07 20 + S08 25 + S09–S15 slowest-peer tail ≈ 3.5 + S16 6 +
S17 4 + S18 4 + S19 28 + S20 2 = **112 µs → −16 %** ✓ (dominant
uncertainty: S07/S08 wake costs and S19 python parse — all three carry
RUNTIME-VERIFY probes).

kevy page (target 159 µs): idx sum + S05 +2 + S06 +2.5 + slowest-peer
(S12/S13/S15 deltas) +3 + S17 +20 = **139 µs → −13 %** ✓ (and the
page−idx delta reconstructs as 25–27 µs vs measured 25 µs ✓).

PG idx (target 124 µs): S01 10 + S02 5 + S03 4 + S04 3 + S05 15 + S11 1 +
S12 3 + S13 2 + S14 22 + S15 12 + S18 3 + executor shutdown/portal drop 5 +
S19 14 + S20 2 = **101 µs → −19 %** ✓ (borderline; the residual is spread
across S03–S05 extended-protocol bookkeeping — acceptable, flagged).

PG page (target 112 µs): idx sum with S14 22→16 (clustered heap pages),
S15 12→13 (3 cols) = **96 µs → −14 %** ✓.

All four sides within ±20 %. The kevy sums deliberately use p50-case
S02/S08 draws; the p99 gap is NOT in these sums — it is O1+O2, which is
exactly the Q1/Q2 split the RFC demanded.

### High-level count assertions to verify at runtime (RFC §2 hard bar)

| assertion | predicted | probe |
|---|---|---|
| `Query::parse` runs per idx query | 17 (16 shards + reduce) | counter in `Query::parse` |
| wake `write()` syscalls per query | ~15 | `strace -c` / counter in `flush_wakes_slow` |
| chunk hits Σ, idx / page | ~20 / ~250 | `IDX.COUNT` + chunk-hit counter |
| hydration store reads per query | 0 | counter in `peek_hydration` non-empty branch |
| naps entered during idx lat phase | ≥ 1 % of samples | `naps_entered` counter (O1) |
| origin drains per query | ~15 Responses in 1–3 batches | `did_inbound` histogram |

---

## Top-N actionable attacks (sorted by estimated µs; Q1 = p50 path, Q2 = tail)

| # | Q | File:line | Concrete change | Est. gain | Semantic class | Blast radius |
|---|---|---|---|---|---|---|
| A1 | Q2 | `kevy-rt/src/uring_reactor.rs:469-471` + `inbox.rs:215,255,277` | Count only `Request`/`RequestBatch` messages toward `last_inbound_batch` (the aggregation signal the nap was built for); `Response`/`ResponseBatch` drains must not arm the nap on the origin | idx p99 −100..−150 µs (384 → ~230–280); mean −15..−25 | none (idle-ladder policy) | ~10 LOC; MUST re-run `legacy_8sh_set` owner-shape bench — the owner drains RequestBatches, which still arm its nap, so predicted neutral there |
| A2 | Q1 | `kevy-rt/src/shard_flush.rs:50-58` + `kevy-uring` | Replace the 15 serialized pipe `write()`s with one submit of 15 `IORING_OP_MSG_RING` SQEs into the peers' rings (peers' `submit_and_wait` completes on the posted CQE); fall back to the pipe on old kernels | idx & page p50 −12..−18 µs | none | ~120 LOC (kevy-uring opcode + park wiring + fallback); all fan-out verbs benefit |
| A3 | Q1 | `kevy/src/cmd_index_reduce/query.rs:148-161` | Borrowed-slice k-way merge: decode chunk hits as `(&[u8] value-view, &[u8] key)` cursors per chunk, run a 16-way selection to the global top-20, materialize ONLY the 20 winners (kills ~460 dead allocs + the 250-element sort on page) | page p50 −15..−20 µs; idx p50 −2 | none (byte-identical reply) | ~60 LOC, reduce-only; claused path untouched |
| A4 | Q1 | `kevy-rt/src/exec_build.rs:101-107`, `message.rs:79` | `Op::Extension { argv: Arc<[Vec<u8>]> }` — one materialization, 16 Arc clones instead of 16 deep clones | idx/page p50 −3..−5 µs | none | ~40 LOC across message/exec/embedded trait |
| A5 | Q2 | `kevy-rt/src/uring_park.rs` (+ diag) | Phase-B gate instrumentation, not an optimization: `naps_entered` + wake-to-run TSC histogram counters behind a debug env; prerequisite for validating A1/A2 per §9 double-gate | — (enables gate 2) | none | ~30 LOC diag |
| A6 | Q1 | `kevy/src/cmd_index_query/args_scalar.rs:67-92` | Parse-once fan-out: origin parses `Query` and ships a compact compiled form in `Op::Extension` (kills 16 redundant parses + 16 bounds computations) | −1..−2 µs p50 now; scales with shard count | none (wire chunk unchanged) | ~150 LOC; touches embedded dispatch parity — defer unless A2/A3 land short |

Not attacked, recorded: kevy's 12.5× page over-fetch (S17) is semantic
(any shard may own all 20 winners); A3 removes its *alloc* cost, the ship
cost (~560 B × 16) stays and is negligible. S14's zero-hydration advantage
is a benchmark-shape property — a FIELDS-carrying shape would add ~20 rows
× 1 hash read (~2–4 µs, still cheaper than PG's heap fetch) and should be
measured before any "kevy wins hydration" claim.

## Gate 2 requirement (pre-Phase-B, methodology §9)

Before ANY attack lands: (1) run the O1 nap counter + `--threads` sweep —
if idx p99 does not move with fan-out width, A1/A2 are misaimed and this
decomp must be redone; (2) `perf record` (dwarf) on the origin shard during
the idx phase — A2's target (`write()` syscall path under
`flush_wakes_slow`) must show ≥ double-digit pp of origin self-time during
the fan-out window, per the "memcpys are the gap" lesson (§8).

## Addendum — the zero-code --threads sweep (same day, decisive)

The cheapest RUNTIME-VERIFY probe ran on lx64 (same box, same
harness, kevy `none`, 5 000 samples per shape per config):

| threads | pk p50/p99 | idx p50/p99 | page p50/p99 |
|---|---|---|---|
| 1 | 15/35 | **36/54** | **43/67** |
| 4 | 19/56 | 58/72 | 71/111 |
| 8 | 21/36 | 63/80 | 100/111 |
| 16 | 24/76 | 104/**371** | 156/201 |

Three verdicts:

1. **The tail hypothesis is confirmed and sharpened.** The
   pathological idx tail exists ONLY at 16 shards (p99/p50: 1.5 /
   1.24 / 1.27 / **3.6**); the 8→16 step adds +291 µs of p99 out of
   nowhere. Whatever arms it (O1 nap is still the leading candidate,
   O2 wakeup scheduling second) engages between 8 and 16 peers —
   the `naps_entered` counter remains the discriminating probe.
2. **The p50 delta is pure fan-out width.** idx p50 walks 36 → 58 →
   63 → 104 µs as width grows 1 → 16; the single-shard path — parse,
   tree walk, encode, reduce of one chunk — beats PG's 126 µs by
   ~3×. There is no per-stage "slow path" to polish at width 1; the
   entire loss versus PG is the price of fanning one ~100 µs query
   across 16 shards and reassembling it.
3. **On the charter's home ground (SME 4–8 cores) kevy already
   wins these shapes**: idx p99 72–80 µs vs PG's 164, page 111 vs
   147 — matching the master plan's earlier cell-level observation.
   The 16-core loss the benchmark reports is real but is a fan-out
   GOVERNANCE problem (the plan's I2 "index-as-key, single-hop"
   direction, plus A1/A2 from the attack table), not a query-path
   speed problem.

Phase B, when gate 2 opens, therefore aims at fan-out governance
first: A1 (nap mis-arming) directly targets the 16-shard cliff; the
structural alternative — routing an indexed lookup to fewer shards
than "all of them" — is the larger prize and needs its own design
round. Nothing here changes the ±20 % stage budgets, which were
built at width 16 and remain the width-16 account.
