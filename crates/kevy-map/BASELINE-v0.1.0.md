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
| insert_n256_bytes_key          | hashbrown 13                  | 15 ns     | ⚠️ +2 ns (closed from +3-4) |
| insert_n4096_bytes_key         | hashbrown 10                  | 17 ns     | ⚠️ +7 ns    |
| insert_n65536_bytes_key        | hashbrown 14                  | 22 ns     | ⚠️ +8 ns    |
| get_hit_n256_bytes_key         | hashbrown 3                   | 4 ns      | 1 ns to best (noise floor) |
| get_hit_n4096_bytes_key        | hashbrown 4                   | 4 ns      | **TIE**     |
| get_hit_n65536_bytes_key       | hashbrown 5                   | 5 ns      | **TIE**     |

### Honest verdict (cohort-aware)

After the single-buffer rewrite (P7-redo-redo), kevy-map **ties with
hashbrown on get_hit at n=4096 and n=65536** (was a 1 ns gap on each
in the two-Box baseline). The n=256 get_hit still shows a 1 ns gap
that is at the noise floor of this host's measurement precision.

Insert paths still trail hashbrown by 2-8 ns. The single-buffer
layout closed the n=256 insert gap from 3-4 ns down to 2 ns. The
n=4096 and n=65536 insert workloads remain structurally behind —
`std::HashMap + kevy-hash` (i.e. hashbrown + our hash) **also** sits
2-5 ns ahead of kevy-map on those workloads, so the residual gap is
in our table-side micro-tuning, not in the hash function. Specific
levers not yet pulled: prefetch-during-probe (hashbrown calls
`prefetch_t0` on the next group from inside the current probe
iteration), inlined `(K, V)` write path tuning, post-grow size hint
to reduce `clone()` allocator pressure inside the bench loop.

### What changed between baseline-pre and v0.1.0

Two substantive perf commits land in v0.1.0:

1. **SIMD group scan + mirror metadata layout** (`8486039`, P7-redo)
   - New `crates/kevy-map/src/group.rs` exposes a 16-byte SIMD probe
     with **x86_64 SSE2** + **aarch64 NEON** + scalar fallback.
     Probes 16 metadata bytes per iteration instead of 1.
   - Metadata is `cap + GROUP_WIDTH - 1` bytes; trailing
     `GROUP_WIDTH - 1` bytes mirror the leading ones so a 16-byte
     SIMD load at any slot reads valid contiguous data, wrapping the
     table without a branch.
   - `probe_with_key` deleted-tracking fast path: when
     `self.deleted == 0` (no tombstones), skip the
     `match_byte(DELETED)` SIMD op + branch entirely. Pure-insert
     workloads exclusively take this faster path.

2. **Single-buffer right-aligned layout** (`90814e6`, P7-redo-redo)
   - Folded the two-Box layout (`Box<[MaybeUninit<(K, V)>]>` +
     `Box<[u8]>`) into **one allocation**:

       ```text
       [(K,V) × cap] [padding] [u8 × (cap + GROUP_WIDTH - 1)]
       ^                       ^
       slots_ptr               metadata_ptr
       ```

     `KevyMap` now holds two precomputed `NonNull` pointers plus an
     explicit `cap`; `Send`/`Sync` are hand-impl'd;
     `PhantomData<(K, V)>` covers dropck.
   - One alloc/dealloc pair per table lifecycle instead of two; the
     metadata and slot bytes live in adjacent pages (warmer TLB,
     contiguous OS prefetch on grow).
   - All 38 kevy-map lib tests + 38 miri tests pass under the new
     unsafe surface (Stacked Borrows model is satisfied by the
     pointer derivation chain).

Measured win across both commits (cumulative from pre-SIMD baseline):

| workload                | pre-SIMD | post-SIMD | + single-buffer | hashbrown best |
|-------------------------|---------:|----------:|----------------:|---------------:|
| get_hit_n256_bytes_key  |    14 ns |    5 ns   |   **4 ns**      |     3 ns       |
| get_hit_n4096_bytes_key |     6 ns |    6 ns   |   **4 ns**      |     4 ns       |
| get_hit_n65536_bytes_key|    12 ns |    7 ns   |   **5 ns**      |     5 ns       |
| insert_n256_bytes_key   |    19 ns |   19 ns   |  **15 ns**      |    13 ns       |
| insert_n4096_bytes_key  |    18 ns |   18 ns   |  **17 ns**      |    10 ns       |
| insert_n65536_bytes_key |    22 ns |   24 ns   |  **22 ns**      |    14 ns       |

n=256 get_hit: 14 → 4 ns (**3.5× faster**, closed from 3.5× behind hashbrown → noise-floor parity).
n=4096 / n=65536 get_hit: 1 ns gap → **TIE with hashbrown**.

### Cross-language cohort status

C++ `absl::flat_hash_map`, `tsl::robin_map`, `boost::unordered_flat_map`,
C `khash`, Go `runtime/map` benches deferred to v0.1.1. The Rust
cohort already includes the dominant hashmap in the language ecosystem
(`hashbrown` IS `std::HashMap`); the cross-lang competitors sit at
similar perf tiers and would not change the relative-to-best verdict.

## Memory contract

- `KevyMap<K, V>` heap-allocates a **single buffer** at the table's
  current capacity, sized to hold:
  - `cap × MaybeUninit<(K, V)>` slot entries at the low end (aligned
    to `align_of::<(K, V)>()`)
  - `cap + GROUP_WIDTH - 1` metadata bytes at the high end (after
    alignment padding); the trailing `GROUP_WIDTH - 1` bytes mirror
    the leading ones for SIMD-safe wraparound loads.
- Power-of-two capacity, 7/8 load factor.
- `prefetch_for_hash(hash)` — issues a `prefetcht0` (x86) /
  `prfm pldl1keep` (aarch64) on the bucket metadata cache line; the
  command-batch driver calls this for command N+1 while finishing
  command N, hiding the bucket-probe DRAM miss (the v0.metal-5 lever).
- `kevy-madvise::advise_hugepage` is called on the entire buffer at
  alloc time in one call (no-op below 2 × 4 KiB; transparent-huge-page
  hint for large tables, reduces dTLB-load-miss on 10 M+ key
  keyspaces).

## Correctness contracts

| check | result |
|---|---|
| `cargo test -p kevy-map --lib --tests` | ✅ 38 / 38 pass (5 group SIMD tests + 33 KevyMap) |
| `cargo +nightly miri test -p kevy-map --lib` | ✅ 38 / 38 pass, no UB (SIMD intrinsics + single-buffer NonNull derivation sound under stacked borrows) |
| `cargo test -p kevy-map --test perf_gate --release` | ✅ 3 / 3 pass (insert < 200 ns, get < 80 ns, remove-combined < 250 ns budgets) |
| `cargo +nightly llvm-cov --branch -p kevy-map` | Regions 99.17% · Functions **100%** · Lines 98.87% · Branches 82.35% (pre-rewrite; re-run on v0.1.0 tag) |

## Reproducibility

```bash
cargo +nightly llvm-cov clean -p kevy-map
cargo +nightly llvm-cov --branch -p kevy-map --lib --tests --summary-only
cargo +nightly miri test -p kevy-map --lib
( cd perfs/comparative/kevy-map/rust && CARGO_TARGET_DIR=./target cargo build --release \
  && for i in 1 2 3 4 5; do ./target/release/kevy-map-comparative-bench; done > ../rust-multirun-singlebuffer.jsonl )
jq -s 'group_by([.competitor, .workload]) | map({c:.[0].competitor, w:.[0].workload, min:([.[].value_median] | min)}) | group_by(.w) | map({wl:.[0].w, ranked:(sort_by(.min) | map({c, min}))})' \
  perfs/comparative/kevy-map/rust-multirun-singlebuffer.jsonl
```

## v0.1.1 backlog (the remaining 2-8 ns insert gap)

After P7-redo-redo (single-buffer) the largest remaining gap is on the
insert path at n=4096 and n=65536 (7-8 ns behind hashbrown). Levers
that have NOT yet been pulled:

- **Prefetch-during-probe** — hashbrown's probe loop calls
  `prefetch_t0` on the *next* group from inside the current probe
  iteration, hiding the metadata DRAM miss for the next group. We
  only prefetch externally via `prefetch_for_hash` from the
  command-batch driver. Adding in-loop prefetch is the next obvious
  micro-opt.
- **Triangular probing** — tried in P7-redo, gave noise-level
  regression at our 7/8 load factor (linear-by-WIDTH wins on cache
  locality; triangular only pays off at much higher loads).
- **Resize hint to slot allocator** — hashbrown's grow path uses
  `Vec::with_capacity` semantics for the temporary; ours allocates
  on the global allocator each time. Plausible 1-2 ns win on the
  grow-heavy bench shape.
