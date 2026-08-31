# Transparent tiering (`[tiering]` / `with_tier_budget*`)

Tiering gives kevy a **RAM budget**: when the keyspace grows past it,
the coldest values spill to a per-boot value log on disk and page back
in on access. Keys and metadata stay in RAM — so **RAM bounds how many
keys you can hold, disk bounds how much data** — and every command
keeps its exact semantics on a cold key. There is no second API, no
"cold namespace", and no cache-miss error to handle: a cold key is an
ordinary key that costs one positional disk read the first time you
materialize it.

```toml
# kevy.toml
[tiering]
budget = "auto"        # or "70%" or "4gb"
```

```rust
let store = Store::open(Config::default()
    .with_dir("/var/lib/kevy")
    .with_tier_budget(3 << 30))?;   // 3 GB RAM budget, embedded
```

Tiering is **off by default**. With no `[tiering]` section (and no
`with_tier_budget*` call) the engine's paths are byte-identical to an
untiered build — that is a gated claim, not a hope.

## What it is — and what it is not

- **It is a RAM budget.** Values past the budget's demote watermark
  move to a value log (`<data>/tier/`), leaving a stub inside the
  entry's existing value slot — no extra heap. The value's RAM is
  fully reclaimed; the key, its TTL, its type, and its LRU history
  stay resident.
- **It is not durability.** The AOF remains the sole durable truth,
  and tiering adds **zero new crash-safety surface by construction**:
  the value log is a **per-boot spill area** — deleted when the store
  opens, rebuilt during replay — so it is never part of what a crash
  can lose. Your crash guarantees are exactly
  [persistence.md](persistence.md)'s; the tiered persistence suite
  additionally pins the two paths where tiering and persistence
  meet (rewrite/snapshot on a mostly-cold store, and boot past the
  budget).
- **It is not eviction.** A demoted key is still present — `EXISTS`
  says 1, `SCAN` returns it, `TTL` counts down, `GET` answers. The
  eight `maxmemory` delete-eviction policies keep their exact
  semantics; if you set `maxmemory` too, it stays the hard
  delete-eviction backstop above the tier budget. Demotion emits
  **zero** keyspace events (it is not a write and not an eviction —
  clients treat `evicted` as key removal); it counts in a separate
  `demotions_total` gauge, never in `evicted_keys`.
- **The index floor is not spillable — know it before picking a
  budget.** Indexes, views and stored `VALUES` columns stay resident
  (they are what makes cold rows cheap to find), so for an index-heavy
  store the reachable RAM saving is bounded by everything **except**
  that floor. A budget below the floor drives the demote target to 0:
  the tier spills everything spillable once and can then do nothing
  more — visible as `tier_effective_target:0` in `INFO`, and new
  index declarations are refused by name. Size the budget from the
  floor formulas in the budget model below, not from the raw data
  size.

## Enabling it

Server — any of the config surfaces, same key:

```toml
[tiering]
budget = "auto"              # 0.70 x the detected memory bound
# budget = "70%"             # percent of the detected bound
# budget = "4gb"             # absolute
# spill_dir = "/fast/nvme"   # optional; default <data_dir>/tier/
```

```console
kevy --tiering-budget 4gb          # CLI
KEVY_TIER_BUDGET=4gb kevy          # env
kevy-cli CONFIG SET tiering-budget 6gb   # live: budget CHANGES only
```

`CONFIG SET tiering-budget` re-resolves on the next shard tick (the
`maxmemory` precedent). Turning tiering on or off, and moving the
spill dir, need a restart — the value-log lifecycle is boot-scoped.

Embedded — behind the `tier` cargo feature (in the default set;
requires `persist`, since the spill area needs a data dir):

```rust
Config::default().with_tier_budget(bytes)      // absolute
Config::default().with_tier_budget_auto()      // 0.70 x detected bound
Config::default().with_tier_budget_percent(50) // percent of the bound
```

A memory-only store (`mem://`, or a `Config` without a data dir) and
the wasm build **reject** the tiering config with a named error at
open — there is no disk to spill to, and a silently-ignored budget
would be a wrong answer wearing a successful boot.

`auto` = 0.70 × the detected memory bound: on Linux,
`min(cgroup v2 memory.max, /proc/meminfo MemAvailable)`; on macOS,
`hw.memsize` (a hand-bound `sysctl` in `kevy-sys`, the workspace's one
sanctioned OS boundary). The bound is **re-probed on the shard tick**,
so a container whose limit is resized adjusts live. A host where no
bound is detectable refuses `auto`/percent at boot, by name — use an
absolute budget.

## The budget model

One budget for the whole process, split evenly across shards. Each
shard demotes toward a unified watermark:

```
demote target = budget·19/20 − index_reserved_bytes − stub_bytes
```

- **Indexes and views are the premium fixed layer** — they are never
  spilled (they are the access paths that make cold rows cheap to
  find), so their bytes are subtracted off the top. If the index floor
  alone exceeds the budget, `IDX.CREATE` / `TABLE.DECLARE` **refuse**
  with a named error rather than admitting an index the budget cannot
  hold.
- **Stub bytes are subtracted too**: the stubs of already-cold keys
  are RAM the budget must carry, so the watermark tightens as the cold
  tier grows. When the fixed floors exceed the 19/20 line the
  effective target saturates to 0 — visible in `INFO`, not hidden.
- The 19/20 factor is the hysteresis band: demotion starts above it
  and stops below it, so the store does not oscillate on the line.
- Spilling is **budgeted**: at most 32 records per demotion call, with
  continuation on the shard tick — a single `SET` never funds an
  unbounded synchronous spill storm.

### RAM per key, hot vs cold

```
hot  key ≈ today's cost (entry + key + value)
cold key ≈ 96 B (entry overhead) + key heap bytes     # value fully reclaimed
```

The cold-key formula is gated at ±20 % against measured RSS
(`bench/memgate.sh`). Two consequences worth planning around, both
from the capacity model:

- **A ~64 B value can never tier profitably** — the stub is about as
  big as the value. Values below the 64-byte spill threshold are
  never spilled. The data:RAM ratio grows linearly with value size;
  every capacity gate names its value size for exactly this reason.
- **Measured, 2026-08-05.** The ratio curve is no longer only a model.
  Same budget and keys, only the value size varied (`INFO Tiering`'s
  `stub_bytes` is the floor doing the work):

  | value size | data:RAM achieved |
  |---:|---:|
  | 256 B | **2.65×** |
  | 1 KiB | **10.43×** |
  | 4 KiB | **39.2×** (full scale, 2 GB budget, 80 GB of data) |

  The floor is ~96 B per entry at a 9-byte key (~143 B at a 48-byte
  key), flat across every scale tested, which makes the ceiling
  predictable: **max data:RAM ≈ value_size / (96 B + key heap)**. That
  predicts 2.67× / 10.7× / 42.7× for the three rows above. An
  interactive version of this formula lives at
  <https://kevy.golia.jp/capacity/>.

  **What that 96 B is matters for where the lever is.** It is the
  keyspace entry (`ENTRY_OVERHEAD`: the inline key cell plus the
  `Entry`), which **every** key pays whether it is tiered or not — the
  cold stub itself is 24 B inline and owns no heap. Tiering returns the
  value and can never return the key that names it, so no tiering knob
  moves this number; only the store's entry layout does, and that
  changes what every key costs in every workload.

  **At 256 B the budget is not merely tight, it is unholdable**:
  `used_memory` crosses a 16 MB budget by 200 000 entries and reaches
  77 MB at 800 000, because demoting a 256 B value frees less than the
  stub it leaves behind. Narrow records are the case to size by hand.
- **Worked example (sized from the model, not measured into it)**:
  10 M rows × ~1 KiB (≈10 GB of data) with 2
  secondary indexes + stored VALUES columns fits a **3 GB** budget:
  stub floor 10 M × ~108 B ≈ 1.1 GB, index floor 10 M × (68 + 68 +
  ~30 VALUES bytes) ≈ 1.7 GB ≈ 2.8 GB ≤ 3 GB. At 4 KiB values the
  ratio gate is ≥ 10× data:RAM (5 M × 4 KiB = 20 GB on a 2 GB
  budget; stub floor ≈ 540 MB). Per-key fixed costs dominate narrow
  rows: size a deployment from the formulas above — the stub and
  index floors are hard lower bounds a budget must clear before any
  value can stay hot.

## Semantics on cold keys

Every command is transparent. The stub carries the value's **type
tag**, so the whole metadata surface answers with **zero disk reads**:

- `SCAN` / `KEYS` / `RANDOMKEY` / `DBSIZE` see cold keys — there is
  one key table, so SCAN's guarantee is unchanged. `SCAN … TYPE t`
  filters from the tag without touching disk.
- `TYPE`, `EXISTS`, `TTL`/`EXPIRE`-family, `RENAME`, `DEL`,
  `PERSIST` never read the value. `DEL`/overwrite on a cold key
  credits the value log's dead-bytes and frees the stub — no read.
- **A `WRONGTYPE` refusal never pays a disk read**: type checks
  resolve from the stub before any I/O (`LPUSH` on a cold string is
  refused as cheaply as on a hot one).
- `SET`-family `NX`/`XX` read existence from the stub;
  `FLUSHALL` clears the map and resets the value log.
- Hash **field TTLs stay RAM-resident** for cold hashes and survive a
  demote/promote round trip; fields that expired while cold are purged
  at promotion.
- `WATCH` is not bumped by a demotion or promotion — tier movement is
  not a write.

These are not narrative claims: the **transparency suite** replays the
same operation sequence against a tiered and an untiered store and asserts
byte-identical replies for semantic commands — order-free replies
(`HGETALL`, `KEYS`, `SCAN`, `RANDOMKEY`, whose element order is map
order by contract) are shape-compared instead — (memory-reporting
commands are shape-compared — they legitimately differ), with
demotion points forced deterministically, never timing-dependent.

### Promotion policy

- A cold value **promotes on its second materializing access**. The
  first cold read serves the bytes straight from the log without
  installing them (a probation mark is stamped); the second installs.
  One stray read of an archival key does not churn the hot tier.
- **Bulk paths never promote**: hydration (`FIELDS`, `VIEW.HYDRATE`),
  index backfill, `PREFIX.DIGEST`, scope moves, exports, and
  snapshot/AOF-rewrite serialization all read cold values through a
  no-promote peek. An `IDX.CREATE` backfill over a fully-cold table
  reads one record per row and installs none of them.
- The zero-copy shared lane (`kevy_get_shared` over the C ABI) reads a
  cold value per call and never promotes — shared-lane readers pay the
  read until a normal access path promotes the key.
- `MEMORY USAGE` on a cold key reports the **stub's** RAM footprint
  (what the key actually costs you now); the original value size rides
  inside the stub for re-accounting at promotion.

## What spills (v1)

Strings (above the 64-byte threshold) and **hashes** — a hash spills
as one record carrying all its fields, which is what makes a cold
table row cost one read, not one per field. Lists, sets, sorted sets
and streams **stay hot** in v1 — a named limitation (collection spill
is on the post-v4 list), not an error: they simply are not demotion
candidates, and the budget arithmetic treats them as permanent hot
bytes.

## `INFO` — the `# Tiering` section

Present only when tiering is enabled (an untiered server's `INFO` is
byte-identical to a build without tiering at all); identical on
server and embedded listener.

| field | meaning |
|---|---|
| `tiering_enabled` | `1` (the section is absent when off) |
| `tier_budget_bytes` | the resolved budget (auto/percent → bytes, live) |
| `tier_effective_target` | `budget·19/20 − reserved − stubs`, saturating at 0 — 0 means the fixed floors alone exceed the watermark |
| `cold_keys` | keys currently demoted |
| `cold_bytes` | original (pre-demotion) bytes of those values |
| `stub_bytes` | RAM the stubs of cold keys occupy |
| `index_reserved_bytes` | the index/view floor subtracted from the watermark |
| `vlog_size_bytes` | value log size on disk |
| `vlog_live_bytes` | bytes still referenced (the rest is compactable) |
| `vlog_files` | value-log segment files |
| `vlog_epoch` | compaction epoch (retired-file counter) |
| `demotions_total` | values spilled since boot |
| `promotions_total` | values paged back in since boot |
| `peek_preads_total` | no-promote cold reads (one per cold **row**) |
| `batch_submissions_total` | batched cold-read submissions (hydration pages) |

`vlog_size_bytes / cold_bytes` is the space-amplification ratio the
acceptance gate clamps at ≤ 2.0×; `peek_preads_total` is how you
verify a hydration page paid one read per row, not per field.

## Performance expectations

Stated honestly, with the measured/pending status of each number:

- **Hot path: structurally unchanged.** The hot-value read/write path
  gains exactly one never-taken match arm; with tiering compiled in
  but off, perfgate's 12 metrics gate at the existing tolerance, and
  with tiering on and the working set fully hot, the new
  `tiered_hotset_*` lines gate the same way. **Baselines pending on
  the bench box** — the gate lines exist and skip with a notice
  until then.
- **Cold point read = one positional read** plus a CRC check and a
  decode. The `kevy-vlog` microbench measures `read_at` at 0.64–14 µs
  across record sizes (dev-box NVMe). The end-to-end SLAs the
  gate clamps — scalar p99 ≤ 100 µs embedded / ≤ 300 µs server,
  whole-hash-row materialization ≤ 200 µs / ≤ 500 µs — are **measured
  on the server side**: the envelope run holds scalar p99 at 79–171 µs
  and whole-hash-row at 145 µs across datasets from 20 GB to 120 GB on
  a 2 GB budget, with latency flat as the dataset grows 6×. The
  embedded numbers remain targets — the envelope drives the server.
- **Embedded holds the shard lock during a cold read.** In-process
  there is no reactor to hand the read to: a cold materialization
  preads under the shard's write lock (the 1-shard default = the
  whole store), stalling that shard's readers for the read's duration
  — ~µs-class on NVMe, potentially ms-class on cloud block storage,
  and proportionally longer for large values. The embedded config
  caps the largest spillable value at **256 KiB by default**
  (`max_spill_value`; `with_max_spill_value(bytes)`, 0 = unlimited) so
  that window is bounded — an over-cap value simply stays hot. The
  server leaves the cap unlimited (thread-per-core shards don't share
  a lock). The drop-lock/pread/relock dance that removes the window
  entirely is designed and explicitly post-v4.
- **Batched hydration: one read per row.** A `FIELDS` hydration page
  or `VIEW.HYDRATE` over cold rows coalesces its reads by log
  position and submits them as one batch (io_uring: linked reads on a
  secondary ring; poller/embedded: an ordered positional-read loop).
  One read decodes **all** requested fields of a row — counters
  assert `preads == cold rows`, never `rows × fields`.
- **Index-only queries touch zero rows** — FILTER/SORT/COUNT answer
  from RAM-resident index columns, so on a fully-cold table they
  perform zero disk reads (counter-asserted). This is the reason to
  declare `VALUES` columns on a tiered table: see
  [tables.md](tables.md).
- **Spill never stalls the reactor unboundedly**: demotion is
  budgeted per call with tick continuation; the stall clamp
  (spill-induced p99 ≤ 1 ms) is its own gate line, also pending its
  bench-box measurement.
- **Idle costs converge to nearly nothing** (v4.1). Two mechanisms,
  both required — "idempotent is not convergent" was the lesson of a
  consumer measuring 300–500× idle CPU with tiering on and turning
  the feature off:
  - The per-tick index/view floor feed is served from a
    **generation cache**: an idle store recomputes nothing, and under
    write load every segment statistic is a running counter (O(1))
    rather than a walk over the index structures.
  - The demote sampler **backs off exponentially** when a tick's
    batch moves nothing while over target — including the
    `effective_target = 0` state, which previously guaranteed one
    full sample walk per tick forever. Any demotion (tick or write
    path) resets the backoff; the write path itself always samples
    immediately, so a fresh spillable value never waits out the
    window (bounded at ~6 s).

## Operational notes

- The value log lives under `<data>/tier/` (or `[tiering] spill_dir`),
  is **deleted at every open**, and is rebuilt as replay spills past
  the watermark. Do not back it up; do not point two stores at one
  spill dir.
- Segments rotate at 256 MiB; a sealed segment compacts when its live
  ratio falls below 50 % (space amplification ≤ 2.0× live cold bytes,
  gated). Compaction respects pins — a snapshot or AOF rewrite in
  flight keeps its segment files readable until it finishes.
- Every record carries a CRC32C; bit rot in the spill area is refused
  at read, not served.
- **Boot with dataset > budget works**: replay checks the watermark as
  it goes and spills inline instead of OOMing before tiering ever runs.
  The same inline demotion rides reshard and replica snapshot-load.
  Measured 2026-08-05 — a 2.3 GB AOF against a 64 MB budget (36×)
  replays clean, all 300 000 rows present, `used_memory` settling at
  35 MB, comfortably inside the bound.

  **RSS is another matter, and this page used to overstate it.** It
  claimed RSS stays ≤ budget × 1.05 throughout boot and called that
  gated; neither was true. Measured peak RSS during that replay was
  **137 MB — 2.15× the budget** — the same allocator overhead the
  capacity sweep measures in steady state, and `tiergate`'s L11 line
  has no measurement body yet (it reads `PENDING`). What holds through
  boot is the **logical** bound, which is what the tier accounts for;
  size the machine from RSS, not from the budget.
- Snapshot / `BGREWRITEAOF` / replication full-sync on a mostly-cold
  store stream cold values from the pinned log **without promoting
  anything** — peak extra RAM is one value, and zero cold values are
  lost from a rewrite. **Measured 2026-08-05, not gated:** 60 000 keys
  with 54 912 of them cold, `BGREWRITEAOF`, restart — all 60 000 back,
  every spot-checked key across the range its full length, AOF clean.
  `tiergate`'s L10 line for this is still `PENDING`, so the claim rests
  on that measurement rather than on CI.
- **`used_memory` ≤ budget × 1.05 sustained** is its own gate line
  (`tiergate` L8), including the auto-probe answering correctly in a
  cgroup container and on bare metal. It is the *logical* bound — the
  gate reports RSS beside it rather than clamping it. Measured
  2026-08-05: `used_memory` 253 MB against a 281 MB cap, with RSS
  488 MB (1.93× the budget) reported. Reading that line as an RSS
  guarantee is the mistake this page made two bullets up.

## Gate status (honesty ledger)

Everything above that is a mechanism claim is covered by tests and
gates that run in this tree (the transparency suite, the tiered
persistence suite, `bench/memgate.sh`, `bench/tiergate.sh`). The **envelope numbers** are measured on the dedicated bench box by
`bench/capacity-envelope.sh`: cold-read p99, vlog space amplification
under churn, and the capacity ratio all have numbers on this page.

`bench/tiergate.sh` **in a fresh checkout still shows those lines
pending**, and that is the design rather than a contradiction: the gate
consumes a results file the bench box produces, so a tree that has not
been handed one has not verified anything. Run the envelope, carry
`bench/.capacity-envelope-results` back, and
`TIERGATE_RUN_ENVELOPE=1 bash bench/tiergate.sh` turns the lines on the
evidence rather than on this paragraph. A partial run writes to its own
file for the same reason — the gate must never be handed a results file
with lines silently missing.

## See also

- [tables.md](tables.md) — the TABLE.* layer tiering was designed
  with: indexes hot, rows cold, index-only queries touching zero rows.
- [persistence.md](persistence.md) — the durability contract that is
  deliberately untouched by tiering.
- [tuning.md](tuning.md) — memory knobs (`maxmemory` and friends)
  that coexist with the tier budget.
- [`bench/tiergate.sh`](../bench/tiergate.sh) /
  [`bench/capacity-envelope.sh`](../bench/capacity-envelope.sh) — the
  acceptance gate and the envelope runner behind every number quoted
  or pended on this page.
