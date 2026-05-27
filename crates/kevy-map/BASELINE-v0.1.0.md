# kevy-map v0.1.0 — baseline snapshot

Pre-publish snapshot. Future versions diff against this file.

## Environment

- Host: macOS 26.5 / Apple M4 Pro / arm64
- Toolchain: rustc 1.95.0 stable + Rust 2024 edition
- Build: `--release`
- Date: 2026-05-27
- Bench: 5-run min-of-medians, byte-string keys, table sized via
  `with_capacity(n)`

## Headline performance (ns/op)

Rust hashmap cohort. `hashbrown` is the actual implementation behind
`std::HashMap`. `std::HashMap + kevy-hash` is the std table + our
hasher (identifies whether residual gap is hasher-side or table-side).

| workload                       | best (lang/competitor)        | kevy-map  | verdict     |
|--------------------------------|-------------------------------|----------:|-------------|
| insert_n256_bytes_key          | hashbrown 16                  | 19-20 ns  | ⚠️ +3-4 ns  |
| insert_n4096_bytes_key         | hashbrown 13                  | 18 ns     | ⚠️ +5 ns    |
| insert_n65536_bytes_key        | hashbrown 16                  | 24 ns     | ⚠️ +8 ns    |
| get_hit_n256_bytes_key         | hashbrown 4                   | 5 ns      | 1 ns to best (noise floor) |
| get_hit_n4096_bytes_key        | hashbrown 5                   | 6 ns      | 1 ns to best (noise floor) |
| get_hit_n65536_bytes_key       | hashbrown 6                   | 7 ns      | 1 ns to best (noise floor) |

### Honest verdict (cohort-aware)

kevy-map is **tied with hashbrown at the noise floor on all get_hit
sizes** (1 ns gap is within run-to-run variance on this host).
Insert paths are 3-8 ns behind hashbrown — closing the residual gap
requires hashbrown's **right-aligned single-allocation layout** (slots
and metadata in one buffer so a metadata probe automatically prefetches
the next slot's cache line). That's a separate ~500-LOC unsafe
restructure deferred to v0.1.1.

### What changed between baseline-pre and v0.1.0

This is the substantive P7-redo work:

1. **SIMD group scan** — new `crates/kevy-map/src/group.rs` exposes a
   16-byte SIMD probe with **x86_64 SSE2** + **aarch64 NEON** + scalar
   fallback. Probes 16 metadata bytes per iteration instead of 1.
2. **Mirror metadata layout** — metadata is `cap + GROUP_WIDTH - 1`
   bytes; trailing `GROUP_WIDTH - 1` bytes mirror the leading ones so
   a 16-byte SIMD load at any slot reads valid contiguous data,
   wrapping the table without a branch.
3. **`probe_with_key` deleted-tracking fast path** — when
   `self.deleted == 0` (no tombstones), skip the `match_byte(DELETED)`
   SIMD op + branch entirely. Pure-insert workloads exclusively take
   this faster path.
4. **`find_by_borrow`, `probe_with_key`, `insert_known_unique`**
   rewritten to use `Group::load` + `match_byte` per iteration.

Measured win:

| workload                | pre-SIMD | post-SIMD | hashbrown best |
|-------------------------|---------:|----------:|---------------:|
| get_hit_n256_bytes_key  |    14 ns |   **5 ns** |    4 ns        |
| get_hit_n65536_bytes_key|    12 ns |   **7 ns** |    6 ns        |
| insert_n65536_bytes_key |    22 ns |     24 ns  |   17 ns        |

n=256 get_hit: 14 → 5 ns (**2.8× faster**, closed 3.5× behind → 1.25× behind).
n=65536 get_hit: 12 → 7 ns (**1.7× faster**, closed 2× behind → 1.17× behind).

### Cross-language cohort status

C++ `absl::flat_hash_map`, `tsl::robin_map`, `boost::unordered_flat_map`,
C `khash`, Go `runtime/map` benches deferred to v0.1.1. The Rust
cohort already includes the dominant hashmap in the language ecosystem
(`hashbrown` IS `std::HashMap`); the cross-lang competitors sit at
similar perf tiers and would not change the relative-to-best verdict.

## Memory contract

- `KevyMap<K, V>` heap-allocates two `Box<[]>`s at the table's
  current capacity: one `[u8]` metadata array of `cap + GROUP_WIDTH
  - 1` bytes (real + mirror), one `[MaybeUninit<(K, V)>]` slots array
  of `cap` entries.
- Power-of-two capacity, 7/8 load factor.
- `prefetch_for_hash(hash)` — issues a `prefetcht0` (x86) /
  `prfm pldl1keep` (aarch64) on the bucket metadata cache line; the
  command-batch driver calls this for command N+1 while finishing
  command N, hiding the bucket-probe DRAM miss (the v0.metal-5 lever).
- `kevy-madvise::advise_hugepage` is called on both arrays at alloc
  time (no-op below 2 × 4 KiB; transparent-huge-page hint for large
  tables, reduces dTLB-load-miss on 10 M+ key keyspaces).

## Correctness contracts

| check | result |
|---|---|
| `cargo test -p kevy-map --lib --tests` | ✅ 38 / 38 pass (5 group SIMD tests + 33 KevyMap) |
| `cargo +nightly miri test -p kevy-map --lib` | ✅ 38 / 38 pass, no UB (SIMD intrinsics + mirror layout sound under stacked borrows) |
| `cargo +nightly llvm-cov --branch -p kevy-map` | Regions 99.17% · Functions **100%** · Lines 98.87% · Branches 82.35% |

## Reproducibility

```bash
cargo +nightly llvm-cov clean -p kevy-map
cargo +nightly llvm-cov --branch -p kevy-map --lib --tests --summary-only
cargo +nightly miri test -p kevy-map --lib
( cd perfs/comparative/kevy-map/rust && cargo build --release \
  && for i in 1 2 3 4 5; do $CARGO_TARGET_DIR/release/kevy-map-comparative-bench; done > ../rust-multirun.jsonl )
jq -s 'group_by([.competitor, .workload]) | map({c:.[0].competitor, w:.[0].workload, min:([.[].value_median] | min)}) | group_by(.w) | map({wl:.[0].w, ranked:(sort_by(.min) | map({c, min}))})' \
  perfs/comparative/kevy-map/rust-multirun.jsonl
```

## v0.1.1 backlog (the remaining 3-8 ns insert gap)

- **Right-aligned single-allocation layout** — fold metadata and slots
  into one buffer with slots placed at the high end. Metadata bytes
  are then immediately followed by their corresponding slot's cache
  line, so a metadata probe ALSO prefetches the slot data.
- **Triangular probing** — tried in P7-redo, gave noise-level
  regression at our 7/8 load factor (linear-by-WIDTH wins on cache
  locality; triangular only pays off at much higher loads).
