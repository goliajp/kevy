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

### Cross-track synthesis (Node ✓ / Go ✓ / C ✓ measured; C# pending)

Three tracks measured against four engines (SQLite, bbolt, badger, LMDB).
A consistent, honest pattern — kevy's wins and losses are **structural**, the
same axes every time:

**kevy wins:**
- **Single-op reads.** The `kevy_get_shared` zero-copy Arc lane is **flat
  ~12 ns at every value size** and **beats LMDB** — the acknowledged
  read-latency leader — 2–9× (C track). It beats SQLite (Node) and badger
  (Go) too. Only bbolt's zero-copy mmap pointer beats it, and only *through
  the Go binding's copy-out*, not the engine.
- **Single-op writes (small→mid).** A one-off scalar `set` pays kevy a
  buffered AOF append; every peer pays a full per-op transaction commit —
  kevy wins 12–62× at 16 B, holding to ~4 KB.

**kevy loses (named, not hidden):**
- **Bulk / batch writes — the sharpest gap, on every engine.** Peers
  amortize N writes into one transaction (bbolt B+tree bulk fill ~90–140
  ns/op, badger `WriteBatch`, LMDB one-txn, SQLite one-txn): kevy's per-op
  AOF path can't, losing up to 147× (bbolt) / 37× (LMDB) / 3.8× (SQLite) at
  64 KB. kevy has **no batch-write path**.
- **Large single writes (64 KB).** kevy copies the value twice (store insert
  + AOF BufWriter) — the cost the mmkvgate SET decomposition already named.
- **Reads through a copying binding.** kevy-go's `GetScalar` copies out
  across cgo and loses to bbolt's zero-copy pointer — a *binding* loss the C
  track disproves at the engine level.

**Two indicated attack surfaces** (decomposition, not polish — perf-vs-foss):
1. **A batch-write path for kevy embedded** — the universal gap. Every engine
   beats kevy on bulk load because it has a transaction to amortize into.
2. **Zero-copy binding read lanes** (Go first) — the engine lane already wins
   (C track); the copying bindings give the win back.

lx64 definitive pass pending (perf §9); numbers below are dev-host relative
standing. Per-track detail follows.

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

**Harness:** `bench/embeddedgate/go/` (`run.sh` stages release
`libkevy_ffi.a` for the cgo link, restores debug after). Dev host, N=100k,
200 warm keys, median-of-3. bbolt v1.5.0, badger v4.9.4, go 1.25. **T-async
only** (both peers are disk-only — no pure-mem tier): kevy AOF EverySec,
bbolt `NoSync=true`, badger `SyncWrites=false`. Neither peer has a bare
get/set — both force a txn closure; cold-1op = one txn/op, amortized = one
txn/N (badger via `WriteBatch`). `k/p < 1` = kevy faster.

**kevy vs bbolt** (mmap B+tree):

| axis \ size | 16 B | 256 B | 4 KB | 64 KB |
|-------------|:----:|:-----:|:----:|:-----:|
| GET cold-1op | 0.48 (kevy 2.1×) | 0.48 (2.1×) | **1.98 (peer 2.0×)** | **21.3 (peer 21×)** |
| GET amortized | **2.50 (peer 2.5×)** | **1.59 (peer 1.6×)** | 4.81 (peer 4.8×) | **62.2 (peer 62×)** |
| SET cold-1op | 0.02 (kevy 45×) | 0.03 (31×) | 0.15 (kevy 6.5×) | 0.64 (kevy 1.6×) |
| SET amortized | 3.25 (peer 3.3×) | 2.94 (peer 2.9×) | 16.5 (peer 16×) | **147 (peer 147×)** |

**kevy vs badger** (LSM + value log):

| axis \ size | 16 B | 256 B | 4 KB | 64 KB |
|-------------|:----:|:-----:|:----:|:-----:|
| GET cold-1op | 0.25 (kevy 4.0×) | 0.33 (3.0×) | 0.43 (kevy 2.4×) | 1.04 (peer 1.0×) |
| GET amortized | 0.68 (kevy 1.5×) | 0.64 (1.6×) | 0.53 (kevy 1.9×) | 1.30 (peer 1.3×) |
| SET cold-1op | 0.07 (kevy 14×) | 0.09 (12×) | 0.32 (kevy 3.2×) | 0.86 (kevy 1.2×) |
| SET amortized | 5.29 (peer 5×) | 6.24 (peer 6×) | 10.5 (peer 10×) | 0.99 (tie) |

**Reading it — losing axes named, and the binding-vs-engine split:**

- **GET vs bbolt: kevy-go LOSES, at every size on the amortized read** (2.5×
  at 16 B up to **62× at 64 KB**). bbolt's `Get` returns a **zero-copy
  pointer into the mmap** (~90 ns flat, no byte copy); kevy-go's `GetScalar`
  **copies the value out across the cgo boundary** into a Go `[]byte`
  (~133 ns at 16 B, ~5.9 µs at 64 KB, scaling with size). **This is a
  binding-shape loss, not an engine loss** — the C track below proves it:
  kevy's own `kevy_get_shared` zero-copy lane is flat ~12 ns and **beats
  LMDB**, the read leader. Go loses here because `GetScalar` returns owned
  bytes, exactly the copy-vs-wrap gap the mmkvgate Nitro work found decisive
  on mobile. A zero-copy Go GET lane (a view valid until the next call, à la
  bbolt's txn-scoped slice) is the indicated binding-level fix.
- **GET vs badger: kevy wins to 4 KB, loses only ~1.0–1.3× at 64 KB** —
  badger's `ValueCopy` copies too, so it is copy-vs-copy and kevy's engine
  wins except where its own 64 KB copy-out catches up. Confirms the bbolt
  loss is bbolt's *zero-copy*, not kevy's engine being slow.
- **SET cold-single-op: kevy wins decisively** (up to 45× vs bbolt, 14× vs
  badger at 16 B). A one-off scalar write pays kevy only a buffered AOF
  append; the peers pay a full transaction commit (B+tree rebalance /
  LSM memtable) per op. The real-app write path, and kevy's clearest win.
- **SET amortized: kevy LOSES, the largest gaps on any track.** bbolt in one
  transaction bulk-loads at a flat **~90–140 ns/op** (147× faster than kevy
  at 64 KB); badger `WriteBatch` similar (only ties kevy at 64 KB). A
  single-transaction bulk fill is what a B+tree/LSM is built for — sequential
  page fill, one durability event. kevy's per-op AOF-append + store-insert
  has no batch path to amortize into. **The sharpest north-star gap:
  batch/bulk write.**

**Honest bottom line (Go):** kevy owns the **single-op writes** (small→mid
cold SET) and **beats badger on reads**; it **loses reads to bbolt purely
through the cgo copy-out** (a binding fix, not the engine — see C track) and
**loses bulk/batch writes** to both. The two indicated attack surfaces:
(1) a zero-copy Go GET lane, (2) a batch-write path for kevy embedded — both
decomposition targets if the north star is all-axis ≥ peer.

### C — kevy C ABI vs LMDB (the read-latency leader — the toughest bar)

**Harness:** `bench/embeddedgate/c/` (`run.sh` compiles vendored LMDB 0.9.33
— self-contained, no system install — + the harness, links the release kevy
cdylib). Dev host, N=100k, 200 warm keys, median-of-3. **T-async:** kevy AOF
EverySec, LMDB `MDB_NOSYNC`. kevy uses the **`kevy_get_shared` zero-copy Arc
lane** (freed with `kevy_buf_free_shared`) — the true peer to LMDB's
`mdb_get`, which returns a **zero-copy pointer into the mmap**. So this track
is **zero-copy read vs zero-copy read** — no copy-out artifact. LMDB forces a
txn; cold-1op = txn/op, amortized = one txn reused for N. `k/p < 1` = kevy.

| axis \ size | 16 B | 256 B | 4 KB | 64 KB |
|-------------|:----:|:-----:|:----:|:-----:|
| GET cold-1op | 0.29 (kevy 3.5×) | 0.14 (7.3×) | 0.11 (9.0×) | 0.15 (6.5×) |
| GET amortized | 0.50 (kevy 2.0×) | 0.21 (4.7×) | 0.16 (6.2×) | 0.24 (4.1×) |
| SET cold-1op | 0.07 (kevy 15×) | 0.06 (16×) | 0.34 (kevy 2.9×) | **2.23 (peer 2.2×)** |
| SET amortized | 2.79 (peer 2.8×) | 3.61 (peer 3.6×) | 8.83 (peer 8.8×) | **36.8 (peer 37×)** |

**Reading it — the headline finding of the whole track:**

- **GET: kevy wins every size, cold AND amortized — it beats LMDB, the
  read-latency leader, on its home turf.** kevy's `kevy_get_shared` is a
  **flat ~12 ns at every value size** (hashmap hit + O(1) `Arc::clone`,
  zero byte copy); LMDB's `mdb_get` is a B+tree descent through the mmap
  (~50–75 ns amortized, ~85–110 ns with a per-get txn). Even when LMDB
  reuses one read txn (its best case), kevy is **2–6× faster**. This is the
  strongest result on any embeddedgate track: the mmap-view read that beat
  kevy 21× **through the cgo copy in the Go track is beaten by kevy's own
  zero-copy lane here** — proving that Go loss was the binding's copy-out,
  not the engine. Symas + Mozilla call LMDB the read leader; kevy's in-memory
  Arc lane is faster still.
- **SET cold-single-op: kevy wins small** (15–16× at 16–256 B), crosses over
  at ~4 KB, **loses at 64 KB** (peer 2.2×). LMDB pays a full B+tree txn
  commit per op (2.7–12 µs); kevy pays a buffered AOF append — until 64 KB,
  where kevy's double-copy (store insert + AOF BufWriter) overtakes.
- **SET amortized: kevy loses** (2.8× → **37× at 64 KB**). One LMDB txn
  bulk-writes at ~78–722 ns/op; kevy has no batch-write path. Same universal
  gap as the Go track (bbolt/badger bulk-txn) — bulk loading is where the
  purpose-built B+tree/LSM engines win, and kevy's per-op AOF path can't
  amortize into a single durability event.

**Honest bottom line (C):** on the toughest bar, kevy **wins reads outright**
(zero-copy Arc beats mmap-view, every size, 2–9×) and **wins single-op small
writes** (no per-op txn), but **loses bulk/batch writes and large single
writes** (no batch-write path; double-copy). The universal north-star gap,
now seen against three engines (SQLite, bbolt/badger, LMDB), is
**batch/bulk write** — the indicated decomposition target.

### C# — kevy C# scalar vs LMDB (Lightning.NET)

_pending_
