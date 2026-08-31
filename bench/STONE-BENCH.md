# STONE-BENCH — stone microbench baseline

Per-crate microbenchmarks over the public API of the high-blast-radius
stone crates (v3.18 T5c; kevy-alloc added by the v5 experiment). This table is the comparison surface for future
stone polish work: re-run, diff against the row, judge the delta against the
recorded stdev band.

## Re-run

```
cargo run -p kevy-bench --release --example stones            # all six
cargo run -p kevy-bench --release --example stones -- map     # one stone
```

Filters: `map` `ring` `config` `text` `store` `vector` `alloc`.
Runner source: `crates/kevy-bench/examples/stones/`.

## Method

- Harness: `kevy_bench::bench` — one untimed warm-up pass, then N timed
  samples; each sample times `inner` iterations and divides. Reported figure
  is the **median sample ± sample stdev** (Bessel-corrected), plus p95/min.
- N ≥ 5 for every row (heavy builds N=5–7, fast ops N=20–50).
- All input data is fixed-seed (SplitMix64) — byte-identical across runs.
- Rows whose closure executes a batch of API calls are divided down to
  per-operation cost (the `per-op basis` column says what one op is).

## Baseline — darwin arm64 + lx64 canonical

- darwin: Apple M4 Max, 16 cores, 64 GiB, macOS 26.5.2; rustc 1.97.0,
  `--release` (lto=fat, codegen-units=1); 2026-07-11, branch
  `feature/v3-18-structure`. Shared dev box with concurrent compile load
  during the run — absolutes drift with load (the harness doc's standing
  caveat); treat the stdev column as the noise band when judging deltas.
- **lx64 (canonical)**: Intel Core i7-10700K (8C/16T), 62 GiB, Debian 13
  (kernel 6.12), rustc 1.97.0, `--release` (lto=fat, codegen-units=1);
  run pinned `taskset -c 0-7`, idle box; 2026-07-11, branch `feature/v4`
  @ f7585650. The lx64 column is the public-comparison figure — same box
  and cores as every kevy perf gate. The darwin column is the dev-box
  companion. Note the ring cross-thread row: the two threads float inside
  the 0-7 pin, so core topology (SMT sibling vs cross-core) sets that
  figure; judge deltas within one machine only.

### kevy-map (`KevyMap<Vec<u8>, u64>`, byte-string keys)

| row | darwin median ± stdev | p95 | min | N | lx64 median ± stdev | per-op basis |
|---|---:|---:|---:|---:|---:|---|
| get hit, n=10k | 4.8 ± 0.4 ns | 5.4 | 3.8 | 30 | 9.9 ± 0.1 ns | one `get` (hit) |
| get miss, n=10k | 3.2 ± 0.4 ns | 3.7 | 2.6 | 30 | 6.8 ± 0.0 ns | one `get` (miss) |
| insert, n=10k | 26.8 ± 4.4 ns | 32.3 | 20.2 | 30 | 52.1 ± 3.8 ns | one `insert` into fresh `with_capacity(n)` map (incl. key clone) |
| get hit, n=1M | 18.7 ± 1.1 ns | 20.5 | 17.7 | 9 | 35.6 ± 0.5 ns | one `get` (hit) |
| get miss, n=1M | 9.5 ± 1.1 ns | 12.0 | 8.4 | 9 | 15.3 ± 0.3 ns | one `get` (miss) |
| insert, n=1M | 61.9 ± 3.8 ns | 65.3 | 53.6 | 7 | 190.2 ± 2.0 ns | one `insert` into fresh `with_capacity(n)` map (incl. key clone) |

### kevy-ring (SPSC `ring::<u64>(1024)`)

| row | darwin median ± stdev | p95 | min | N | lx64 median ± stdev | per-op basis |
|---|---:|---:|---:|---:|---:|---|
| push+pop, same thread | 2.0 ± 0.0 ns | 2.0 | 2.0 | 50 | 1.0 ± 0.0 ns | one push+pop round-trip |
| cross-thread SPSC | 10.2 ± 2.5 ns | 13.6 | 7.1 | 7 | 1.5 ± 0.1 ns | one item across cores (5M items/sample, fresh ring+thread per sample) |

### kevy-config (default-schema doc: 1121 bytes, 14 sections)

| row | darwin median ± stdev | p95 | min | N | lx64 median ± stdev | per-op basis |
|---|---:|---:|---:|---:|---:|---|
| `from_toml_str` | 5.10 ± 0.51 µs | 6.20 | 4.80 | 30 | 10.17 ± 0.36 µs | parse the full-section doc |
| `to_toml_string` | 2.62 ± 0.37 µs | 3.16 | 1.94 | 30 | 3.21 ± 0.12 µs | serialize a parsed `Config` |

### kevy-text (mixed CJK+ASCII, fixed-seed docs)

| row | darwin median ± stdev | p95 | min | N | lx64 median ± stdev | per-op basis |
|---|---:|---:|---:|---:|---:|---|
| tokenize 1068 B | 5.07 ± 0.39 µs | 5.58 | 3.95 | 30 | 10.10 ± 0.12 µs | one `tokenize` call |
| segment build 10k docs | 3.57 ± 0.20 µs | 3.74 | 3.13 | 7 | 5.04 ± 0.01 µs | one `apply` (per doc, fresh `TextSegment` per sample) |
| query limit=10 | 134.2 ± 13.2 µs | 165.0 | 111.0 | 30 | 155.9 ± 0.5 µs | one `matches` ("分布式缓存 kevy", ~1k matching docs, 10 returned) |

### kevy-store (zset; inline encoding holds ≤ 2 short members)

| row | darwin median ± stdev | p95 | min | N | lx64 median ± stdev | per-op basis |
|---|---:|---:|---:|---:|---:|---|
| zadd 1k members, one key (tree) | 114.2 ± 18.9 ns | 163.5 | 104.0 | 20 | 204.8 ± 9.4 ns | one single-pair `zadd` (fresh `Store` per sample) |
| zrange 0 -1 (1k members) | 14.65 ± 1.26 µs | 17.28 | 13.49 | 30 | 34.53 ± 0.53 µs | one full-range `zrange` |
| zadd 2 short members/key (inline) | 65.8 ± 14.6 ns | 98.0 | 48.8 | 20 | 55.7 ± 1.0 ns | one `zadd`, 500 keys × 2 members — stays inline |
| zadd 3 members/key (promote mix) | 101.4 ± 33.4 ns | 205.3 | 93.9 | 20 | 184.9 ± 4.3 ns | one `zadd`, 334 keys × 3 members — every 3rd promotes inline→tree |

### kevy-vector (HNSW, 128d, default params M=16 / efc=200 / cosine)

| row | darwin median ± stdev | p95 | min | N | lx64 median ± stdev | per-op basis |
|---|---:|---:|---:|---:|---:|---|
| insert 10k × 128d | 283.2 ± 16.9 µs | 318.1 | 274.4 | 5 | 425.4 ± 5.0 µs | one `apply` insert (per insert, fresh graph per sample; incl. 512 B vec clone) |
| knn@10, ef default | 76.9 ± 5.4 µs | 83.6 | 65.9 | 30 | 125.9 ± 0.9 µs | one `knn(q, 10, 0)` on the 10k graph (ef=0 → max(4k, 100)) |

### kevy-alloc (v5 experiment; per-shard heap, 400 B is the PG-comparison value size)

Two columns because the question here is not "how fast" in the abstract:
an allocator has no off switch, so a fast path slower than the system
one would show up on every SET, GET and published message. The system
allocator is the thing being replaced, so it is measured beside us on
identical shapes.

| row | darwin median ± stdev | p95 | min | N | lx64 median ± stdev | per-op basis |
|---|---:|---:|---:|---:|---:|---|
| alloc+free 64 B (kevy-alloc) | 5.0 ± 0.0 ns | 5.0 | 5.0 | 30 | pending | one alloc + one dealloc |
| alloc+free 64 B (system) | 10.0 ± 2.0 ns | 15.0 | 8.0 | 30 | pending | same |
| alloc+free 400 B (kevy-alloc) | 5.0 ± 0.0 ns | 5.0 | 5.0 | 30 | pending | same |
| alloc+free 400 B (system) | 18.0 ± 2.0 ns | 24.0 | 16.0 | 30 | pending | same |
| alloc+free 4096 B (kevy-alloc) | 5.0 ± 1.0 ns | 7.0 | 4.0 | 30 | pending | same |
| alloc+free 4096 B (system) | 16.0 ± 2.0 ns | 21.0 | 13.0 | 30 | pending | same |
| churn 4096×400 B interleaved free (kevy-alloc) | 3.8 ± 0.1 ns | 4.1 | 3.5 | 10 | pending | one alloc+free of the 4096; every other slot freed first |
| churn 4096×400 B interleaved free (system) | 19.5 ± 2.4 ns | 24.7 | 18.3 | 10 | pending | same |
| the same **plus returning the pages** (kevy-alloc) | 29.3 ± 2.9 ns | 32.3 | 23.0 | 10 | pending | churn + `reclaim()` |
| reclaim sweep, nothing to return | 18.0 ± 2.0 ns | 25.0 | 18.0 | 20 | pending | one `reclaim()` call on an already-swept heap |

The page-return row has no system column deliberately: there is nothing
to compare it against, because that is the operation glibc cannot perform
at any price.
Folding it into the churn row instead — which the first draft of this
bench did — timed our reclaim against their nothing and read as a 1.5×
loss that was really a missing column.

## History

| date | machine | change |
|---|---|---|
| 2026-07-11 | darwin arm64 (M4 Max) | first cut (v3.18 T5c); lx64 pending |
| 2026-07-11 | lx64 (i7-10700K) | canonical Linux column lands (v4 T4 K-403); pending marker retired |
| 2026-07-26 | darwin arm64 (M4 Max) | kevy-alloc joins (v5 T1), measured beside the system allocator; lx64 column pending |
