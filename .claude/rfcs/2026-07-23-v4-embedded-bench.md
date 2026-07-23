# RFC — v4 embedded bench (kevy vs per-language native embedded stores)

**Date:** 2026-07-23 · **Roadmap:** v4 t4 item 3 (embedded bench RFC) ·
**Status:** DRAFT for the ledger; competitor set + axes below are the gate
the roadmap names ("竞品名单+轴先 RFC → 拍板后跑"). Written from the
2026-07-23 competitive research pass (four sourced reports, all figures
cited; marketing vs third-party flagged).

This RFC picks **which competitor per language** and **which axes**, and —
the harder half — how to make the comparison **fair and honest** when the
competitors span three storage models (mmap B+tree, LSM, SQL) and two
execution models (sync / async). The north star (roadmap) is to beat the
native embedded store on every axis; the honest starting point is that we
do not yet know, because no such head-to-head has been run.

The companion to the already-shipped **mobile** track (`bench/mmkvgate/`,
kevy vs MMKV, real-device measured, audit exhausted) — this is the
**server-language embedded** track that had no prior art.

---

## 1. The reference — what kevy's embedded scalar path actually is

Measured, local grounding (see `bench/PERF-FINDING-2026-07-16-embedded-get-scalar-vs-resp.md`):

- **API shape:** a bare synchronous `get(key) → bytes|nil` / `set(key, val)`.
  No transaction, no prepared statement, no bucket. Per language:
  - Go: `DB.GetScalar([]byte) → (value, ok, err)` / `DB.SetScalar(key, val, ttlMs)`
  - Node: `db.get(key)` / `db.set(key, value, {ttlMs})` → `getScalar`/`setScalar` native
  - C#: `KevyDb` binary scalar `byte[]`/`ReadOnlySpan<byte>` get/set
  - C: `kevy_get` / `kevy_get_shared` (zero-copy `Arc` lane) / `kevy_set`
- **GET** is a hashmap read behind a per-shard `RwLock` (read guard when
  `maxmemory==0`). 16 B ≈ 230–360 ns FFI. Large-value GET is a zero-copy
  `Arc::clone` (O(1), no byte copy) on the shared lane.
- **SET** is store insert (flat ~233 ns) + AOF append. The AOF append is
  **~90 % of SET cost**; durability tier picks how much that append costs.
- **Read scaling is bounded by shard count, not lock-free** (honest limit,
  already documented) — so the bench is single-threaded per-op latency
  first; a threaded-scaling axis is secondary and must not over-claim.

kevy's edge candidates going in: no per-op transaction/statement setup;
O(1) large-GET clone; in-memory store insert that scales past append+writeback
for larger writes (the mmkvgate real-device SET crossover confirmed this
shape). kevy's structural disadvantage: everything competes with an
**mmap read = pointer-into-page-cache** (LMDB / bbolt / MMKV) which is
zero-syscall and zero-copy on read; kevy's store copy-out (small values)
and AOF write path (all SET) are where the competitors can win.

---

## 2. Competitor selection per language (with one correction to the roadmap list)

The roadmap candidate list was `Go vs bbolt/badger; Node vs
better-sqlite3/classic-level; C# vs LiteDB; C vs lmdb`. Research refines it:

| Lang | PRIMARY peer (fair KV, matched shape) | Secondary / contrast | Dropped / demoted |
|------|----------------------------------------|----------------------|-------------------|
| **Go** | **bbolt** (`go.etcd.io/bbolt` v1.5.0) — mmap B+tree, read-latency profile closest to kevy | **badger** v4.9.4 — LSM+vlog, append/WAL durability closest to kevy's AOF; brackets the B+tree-vs-LSM axis | — (keep both; they bracket the axis) |
| **Node** | **better-sqlite3** v13.0.1 — **synchronous**, same execution model as kevy; 8.65M weekly downloads = the real bar | **classic-level** v3.0.0 — **async** (Promise-only); a labeled cross-model reference point, NOT a latency peer | — |
| **C#** | **LMDB via Lightning.NET** (`LightningDB` 0.22.0 over LMDB 0.9.33) — native `mdb_put`/`mdb_get`, synchronous, the real KV peer | (LiteDB as an optional *document-store* reference only) | **LiteDB demoted from primary** — see below |
| **C** | **LMDB** 0.9.33 — the canonical embedded KV, read-latency leader (Symas + Mozilla corroborated); the toughest, most honorable bar | — | — |

### The LiteDB correction (a category mismatch, not a fairness tweak)

The roadmap said "C# vs LiteDB". Research shows **LiteDB has no native
key→value path** — it is a BSON *document* store, collections-only
(`GetCollection<T>().Insert()/.FindOne()`). Benchmarking kevy's scalar
`get(k)`/`set(k,v)` against LiteDB's collection insert + document
serialization + index maintenance measures **two different categories of
work** — LiteDB pays BSON encode + `_id` index upkeep for what kevy does as
a hashmap put. A win there would be an artifact of the shape mismatch, not a
real result, and would violate the honesty discipline (§6).

The fair C# KV peer is **LMDB through Lightning.NET** — native
`mdb_put`/`mdb_get`, synchronous, byte keys/values, the same shape as kevy's
C# scalar path. This also means **LMDB is measured twice** (C direct, C# via
Lightning.NET), which is efficient and lets us separate the .NET binding tax
from the engine — the same "engine vs binding overhead" split the mmkvgate
Nitro work found decisive on mobile.

LiteDB may appear as a clearly-labeled **document-store reference row**
(kevy-as-document-store via a `{_id,value}` collection would be the
apples-to-apples counterpart) — but it is **not** the KV bar. Recommend
deferring the LiteDB document comparison unless the user wants it; it is a
different product question (document store vs KV), not the t4 north star.

---

## 3. Fairness framework — the hard half

Four competitors, three storage models, two execution models. A bare
"ops/sec" table is dishonest unless these four axes are controlled. Each is
a *labeled* dimension, not a hidden assumption.

### 3.1 Durability tier (compare WITHIN a tier only — never across)

The single biggest way to lie with this bench is to run kevy `mem://` (no
disk) against a competitor's fsync-on-commit and call it a win. So the
ledger reports **per tier**, and a kevy config is only ever compared to a
competitor config in the **same** tier:

| Tier | kevy config | bbolt | badger | better-sqlite3 | classic-level | LMDB |
|------|-------------|-------|--------|----------------|---------------|------|
| **T-mem** (no disk durability) | `mem://` | — (all are disk-backed) | — | `:memory:` | (mem down not comparable) | — |
| **T-async** (OS-flush, bounded crash window) | AOF `EverySec` | `NoSync` | `SyncWrites=false` (default) | WAL + `synchronous=NORMAL` | `{sync:false}` (default) | `MDB_NOSYNC`/`MDB_NOMETASYNC` |
| **T-fsync** (fsync per commit) | AOF `Always` | default (fsync-on-commit) | `SyncWrites=true` | `synchronous=FULL` | `{sync:true}` | default (fsync-on-commit) |

The **headline tier is T-async** — it is what MMKV/mmap engines do by
default and what a mobile/desktop app actually runs; it is also the tier the
mmkvgate lx64 numbers used (apples-to-apples: both flush to the OS, neither
fsyncs per write). T-fsync is the "durable-write" honesty column. T-mem
isolates the pure engine (read path, store insert) from the disk path.

### 3.2 Execution model (sync vs async — labeled, not conflated)

kevy scalar is **synchronous**. So are better-sqlite3, LMDB, LiteDB,
and (per-op, inside a txn closure) bbolt/badger. **classic-level is async**
(Promise-only). Its numbers go in a separate labeled block — an `await
db.get()` per op measures event-loop turn cost, not KV latency, and must not
sit in the same table as a sync call. It is a "here is what the async
LevelDB path costs" reference, not a peer.

### 3.3 API-shape axis (cold single-op vs amortized)

kevy's scalar call has **no per-op setup**. The Go/C/C# KV peers force a
**transaction** (bbolt/badger `db.View`/`db.Update` closure; LMDB
`mdb_txn_begin`/`commit`); better-sqlite3 needs a **prepared statement**.
There are two honest numbers, both reported:

- **Cold single-op** — open a txn / begin+commit around **one** get/set (or
  reuse a prepared statement but commit per op). This is what a one-off
  scalar store call costs; kevy's simpler API shows here as a real
  ergonomic + latency edge (no txn tax).
- **Amortized** — one read txn wrapping N gets, one write txn wrapping N
  sets (or a prepared statement reused in a tight loop). This is the
  competitor's best case and the fair "tight loop" number. kevy's figure is
  **the same in both** (no txn to amortize) — itself a finding to state, not
  hide.

Both columns appear. Reporting only "amortized" flatters the competitors'
setup cost away; reporting only "cold" flatters kevy. Report both.

### 3.4 Value-size sweep + cold-start

- **Value sizes:** 16 B, 256 B, 4 KB, 64 KB — the same sweep as mmkvgate.
  The GET crossover (mmap-view zero-copy vs kevy `Arc::clone`) and the SET
  crossover (append+writeback vs in-memory insert) both live in this sweep;
  a single size hides them.
- **Cold-start / load:** time to open an existing store of K keys and read
  the first value — mmap engines (LMDB/bbolt) fault pages lazily; kevy
  replays its AOF; badger replays its vlog; SQLite opens the file. This is
  the "startup load" axis the mmkvgate ledger named for mobile, here for
  server languages.

---

## 4. Axes matrix (what the ledger measures)

Per language, per durability tier, per value size:

| Axis | kevy | competitor | note |
|------|------|-----------|------|
| GET cold single-op | scalar get | txn-per-get / stmt+commit | kevy no-txn edge shows |
| GET amortized | scalar get | one read txn × N | competitor best case |
| SET cold single-op | scalar set | txn-per-set / commit-per-op | |
| SET amortized | scalar set | one write txn × N | |
| Cold-start load (K keys) | AOF replay + first read | mmap+fault / vlog replay / file open | startup axis |
| (secondary) threaded GET scaling | shard-bounded | per-engine reader model | **must not over-claim** — kevy read scaling is shard-bounded, §1 |

Losing axes are **named in the ledger, not hidden** (§6). Each loss that is
architectural (e.g. mmap-view zero-copy read vs locked-store copy-out — the
same one the mmkvgate real-device GET small-value floor named) is stated as
architectural with its mechanism, not spun.

---

## 5. Measurement protocol

- **Harnesses:** one per language, committed under `bench/embeddedgate/<lang>/`,
  each self-contained and runnable. Every harness measures kevy's scalar
  path and the competitor(s) **in the same process / same host / same run**,
  interleaved (candidate-vs-reference in one invocation), the perfgate
  discipline — so box drift cancels.
- **Local (dev) runs** give **relative standing** (kevy vs peer in the same
  env), meaningful the way the mmkvgate simulator numbers were — direction
  and crossover points are trustworthy; absolute ns are not an SLA.
- **Precise numbers** need a clean box. Per the perf methodology §9 (do not
  trust a shared/noisy box for absolute SLA), the definitive figures run on
  **lx64** (the mmkvgate SET refutation lived exactly here: the sim inflated
  kevy's write path; real ext4 flipped the result). median-of-5 + sample
  stdev; competitor version pinned and recorded; gap ≤ max(stdev) = NOISE.
- **Durability parity is asserted, not assumed** — each harness prints the
  exact durability config of both sides so the tier match is auditable.
- **Toolchains** (all present locally 2026-07-23): go 1.26.5, node 26.5,
  dotnet 8.0.129, clang 21, cargo 1.97.1. All four tracks buildable +
  runnable on the dev host; lx64 for the definitive pass.

---

## 6. Honesty commitments (the perf methodology, applied)

- **Durability is never mismatched to flatter kevy.** Cross-tier
  comparisons do not appear in a verdict column. If kevy `mem://` is shown
  against a disk engine, it is labeled T-mem-vs-disk and called what it is.
- **Losing axes are named.** The north star is all-axis ≥ peer; the ledger
  starts by admitting where we lose and why. An axis lost for an
  architectural reason (mmap-view read) is stated as such, not attacked with
  a symptom patch.
- **Marketing vs measured is flagged.** The research found that MMKV,
  react-native-mmkv, LiteDB, and Dgraph's badger-vs-bolt numbers are all
  **vendor/marketing** (chart images, self-authored, old engines); the only
  independent absolute figure sourced was better-sqlite3 ≈ 1.22M ops/s
  indexed get (SQG 2026-01-19) and LMDB's read dominance (Symas + Mozilla).
  **Our measured head-to-head is the value-add** — almost none of these
  publish a hard point-get/point-set ops/sec a consumer can trust.
- **Attack losing axes by decomposition, not polish** (perf-vs-foss): 2
  rounds of polish without moving the needle → decompose against the
  competitor's source (LMDB `mdb_get`, better-sqlite3's prepared-stmt path).
  The mmkvgate mmap-AOF refutation is the cautionary precedent: an
  architectural "fix" (mmap append) was built, measured, and refuted — do
  not assume, measure.

---

## 7. Deliverable

- `bench/EMBEDDED-LEDGER.md` — the head-to-head ledger, per language / tier /
  size, losing axes named. Structured like `bench/mmkvgate/LEDGER.md`.
- `bench/embeddedgate/<lang>/` — one committed, runnable harness per track
  (go / node / csharp / c). Kevy side + competitor side, interleaved, prints
  durability config.
- Roadmap t4 item 3 advances from "RFC-gated" to "RFC written, harnesses
  built, relative-standing measured; lx64 definitive pass = the run step".

## 8. Open decisions (reserved for the user)

1. **LiteDB document-store row** — include a labeled document-store
   comparison (kevy `{_id,value}` collection vs LiteDB) or defer? Recommend
   **defer** (different product question from the KV north star).
2. **classic-level (async) inclusion** — keep as a labeled cross-model
   reference or drop? Recommend **keep, clearly separated** (it is the most
   common Node embedded-KV, worth a data point even though not a latency peer).
3. **lx64 definitive run** — the relative-standing pass is autorun; the
   definitive-SLA lx64 pass is a box-time decision (shared box; perf
   methodology §9). Recommend running it once the harnesses are green
   locally, same as the mmkvgate lx64 pass.
