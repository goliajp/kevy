# EMBEDDED-LEDGER — kevy embedded scalar path vs per-language native stores

The server-language companion to `bench/mmkvgate/LEDGER.md` (the mobile
track, kevy vs MMKV, real-device measured). This ledger records kevy's
embedded scalar `get`/`set` head-to-head against each language's native
embedded store, **losing axes named, not hidden** — the north star
(roadmap t4) is to beat the native store on every axis; the honest starting
point is that no such comparison had been run.

Design + competitor selection + fairness framework:
`.claude/rfcs/2026-07-23-v4-embedded-bench.md`. Read it first — it defines
the durability tiers, the sync-vs-async split, the cold-single-op vs
amortized axes, and why LiteDB is a document-store reference rather than the
C# KV peer (LMDB via Lightning.NET is).

## Rules (from the RFC — do not violate in a table)

- **Compare within a durability tier only.** T-mem / T-async / T-fsync
  never cross in a verdict column. Each harness prints both sides' exact
  durability config so the tier match is auditable.
- **Sync and async never share a table.** classic-level (async) is a
  labeled reference block, not a latency peer.
- **Report cold-single-op AND amortized.** kevy's scalar number is the same
  in both (no txn to amortize); the competitors' differ (txn/stmt setup).
- **Relative standing from dev-host runs; absolute SLA from lx64.** Per perf
  methodology §9 — the mmkvgate SET refutation (sim inflated the write path,
  real ext4 flipped it) is the precedent.

## Competitor versions (pinned as measured)

| Lang | Peer | Version | Model | Sourced |
|------|------|---------|-------|---------|
| Go | bbolt (`go.etcd.io/bbolt`) | v1.5.0 (2026-06-03) | mmap B+tree, 1 writer | pkg.go.dev |
| Go | badger (`github.com/dgraph-io/badger/v4`) | v4.9.4 (2026-07-08) | LSM+vlog, SSI | pkg.go.dev |
| Node | better-sqlite3 | v13.0.1 (2026-07-21) | SQLite, **sync** | npm |
| Node | classic-level | v3.0.0 (2025-04-20) | LevelDB, **async** | npm |
| C# | LightningDB (LMDB) | 0.22.0 (2026-07-05) / LMDB 0.9.33 | mmap COW B+tree | nuget |
| C | LMDB | 0.9.33 (2024-05-21) | mmap COW B+tree | openldap |

---

## Results

_Pending — harnesses under `bench/embeddedgate/<lang>/`. Each track's table
lands here as it is measured (dev-host relative standing first, lx64
definitive pass second). Losing axes named per the rules above._

### Node — kevy-node vs better-sqlite3 (sync) / classic-level (async)

**Harness:** `bench/embeddedgate/node/bench.js`. Dev host (M-series mac),
N=100k ops/measurement, 200 warm keys, median-of-3, kevy-napi **release**
(`KEVY_NAPI_LIB=target/release/libkevy_napi.dylib`), better-sqlite3 13.0.1,
classic-level 3.0.0, node v26.5.0. **Relative standing, not SLA** — mac dev
host; the definitive pass is lx64. Interleaved in one process so box drift
cancels. kevy's scalar has no per-op txn, so kevy cold == amortized;
better-sqlite3 setCold = autocommit/op, setAmort = one txn wrapping N.

`k/p` = kevy_ns / peer_ns; **< 1 means kevy faster.**

**T-mem** (kevy `mem://` vs sqlite `:memory:` — no disk durability):

| axis \ size | 16 B | 256 B | 4 KB | 64 KB |
|-------------|:----:|:-----:|:----:|:-----:|
| GET | 0.70 (kevy 1.4×) | 0.66 (1.5×) | 0.61 (1.6×) | 0.50 (**2.0×**) |
| SET cold-1op | 0.19 (kevy 5.2×) | 0.20 (5.0×) | 0.18 (5.4×) | 0.23 (4.3×) |
| SET amortized | 0.39 (kevy 2.6×) | 0.39 (2.6×) | 0.39 (2.6×) | 0.07 (14×)* |

**T-async** (kevy AOF EverySec vs sqlite WAL+`synchronous=NORMAL` — the
headline tier, what a real app runs; both OS-flush, neither fsyncs per op):

| axis \ size | 16 B | 256 B | 4 KB | 64 KB |
|-------------|:----:|:-----:|:----:|:-----:|
| GET | 0.58 (kevy 1.7×) | 0.25 (4.0×) | 0.35 (2.8×) | 0.29 (3.4×) |
| SET cold-1op | 0.04 (kevy 28×) | 0.03 (30×) | 0.07 (15×) | 0.23 (4.3×) |
| SET amortized | 0.63 (kevy 1.6×) | 0.71 (kevy 1.4×) | **1.75 (peer 1.8×)** | **3.80 (peer 3.8×)** |

**Reading it — losing axes named:**

- **GET: kevy wins every size and tier**, lead widening with value size
  (2.0× at 64 KB T-mem). Mechanism: kevy's large-GET is a zero-copy
  `Arc::clone` (O(1)); SQLite copies the row out. Same shape the mmkvgate
  real-device GET crossover showed vs MMKV.
- **SET cold-single-op: kevy wins crushingly** (up to 28× on T-async). A
  one-off scalar `set` is what most app writes are; SQLite pays a per-op
  WAL commit (T-async) or journal (T-mem), kevy's AOF append is buffered
  (no per-op fsync at EverySec). This is the no-per-op-txn edge the RFC
  predicted, measured.
- **SET amortized, large values: kevy LOSES** — T-async 4 KB peer 1.8×,
  64 KB peer 3.8×. When SQLite batches all writes into one transaction (one
  fsync at commit) its WAL append of large blobs beats kevy's per-op AOF
  append. This is **the known architectural SET cost**: kevy copies the
  value twice per SET (into the store + into the AOF BufWriter) and the
  page-cache copy scales with size — exactly what `bench/mmkvgate/LEDGER.md`
  decomposition #2 named and what the mmap-AOF attack tried (and was
  refuted) to close. kevy still **wins amortized SET at ≤256 B** (1.4–1.6×);
  the crossover is at ~4 KB. Named, not hidden — the north-star gap on this
  track is large-value batched writes.
- **classic-level (async): ~10× slower than both sync engines** (GET
  6.8–28 µs, SET 9–120 µs) — an `await` per op measures event-loop turn
  cost, not KV latency. Confirms the RFC's sync/async split: it is a
  cross-model reference, not a latency peer.

**Caveats (honesty):**
- `*` T-mem amortized 64 KB (sqlite 39 µs) is a SQLite
  transaction-rollback-accumulation artifact (100k × 64 KB overwrites in one
  in-memory transaction), not a clean 14× — the T-mem SET cold-1op 64 KB
  (4.3×) is the representative large-value number there.
- T-async GET 16 B (kevy 2432 ns) carries first-open page-cache warmup on
  the file-backed store; 256 B+ settles to 610–712 ns. Direction holds.
- Definitive ns need lx64 (perf §9); these are dev-host relative standing.

### Go — kevy-go vs bbolt / badger

_pending_

### C — kevy C ABI vs LMDB

_pending_

### C# — kevy C# scalar vs LMDB (Lightning.NET)

_pending_
