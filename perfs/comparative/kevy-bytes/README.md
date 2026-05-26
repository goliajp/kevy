# kevy-bytes — cross-language comparative bench

## Stone identity

- name: `kevy-bytes`
- description: 24-byte byte-string with inline SSO (≤ 22 bytes inline, heap above)
- size: `size_of::<SmallBytes>() == 24` (const-asserted in `crates/kevy-bytes/src/lib.rs`)

## Primary metrics

Three "lower-is-better" metrics, segmented by SSO state (inline vs heap):

- **`clone_*`** — ns/op to clone an owned value. Hot path for shared
  data structures that pass owned copies.
- **`eq_*`** — ns/op to compare two equal values. Hot path for hashmap
  lookup, etc.
- **`from_bytes_*` / `from_str_*`** — ns/op to construct from a borrowed
  slice. Hot path on parser → owned conversion.

## Competitor cohort (with semantic grouping)

**Owned-copy cohort** — semantically equivalent to kevy-bytes (full byte
ownership; clone = alloc + memcpy when heap-resident):

| language | competitor | crate / package | SSO threshold |
|---|---|---|---|
| rust | `Vec<u8>`         | std            | none |
| rust | `String`          | std            | none |
| rust | `smartstring::alias::String` | `smartstring` 1.0 | 23 bytes |
| rust | `compact_str::CompactString` | `compact_str` 0.9 | 24 bytes (last-byte tag) |
| c++  | `std::string`     | libc++ (Apple Clang) | 22 bytes |
| c    | `sds`             | vendored from redis  | none |
| go   | `[]byte` + `string` | runtime              | none |

**Shared-copy cohort** — clone is reference-count increment, NOT a copy.
Listed for completeness but **gate is segregated**: kevy-bytes only competes
against owned-copy types. Treating Arc-shared as "the best" would penalise
the owned-semantic stone for choosing a different concurrency model.

| language | competitor | crate / package | clone semantic |
|---|---|---|---|
| rust | `smol_str::SmolStr` | `smol_str` 0.3 | `Arc<str>` clone = atomic refcount inc |

## Gate

```
median(kevy-bytes) ≤ min(median over owned-copy cohort)
```

…on every (workload, SSO-state) combination. Heap clone is the hot fight;
inline clone is at noise floor for every SSO competitor.

## Results history

| date       | kevy version | workload          | best owned competitor | competitor median | kevy median | pass? |
|------------|--------------|-------------------|-----------------------|------------------:|------------:|-------|
| 2026-05-27 | v0.1.0       | clone_inline_12B  | compact_str           | 0 ns (p95 1)      | 3 ns (p95 4) | tie/close |
| 2026-05-27 | v0.1.0       | clone_heap_64B    | Vec<u8>               | 23 ns             | 36 ns        | ❌ 1.57× slow |
| 2026-05-27 | v0.1.0       | eq_inline_12B     | (tied, const-folded)  | 0 ns              | 0 ns         | invalid measurement |
| 2026-05-27 | v0.1.0       | eq_heap_64B       | (tied, const-folded)  | 0 ns              | 0 ns         | invalid measurement |
| 2026-05-27 | v0.1.0       | from_bytes_inline_12B | …                 | …                 | …            | (see jsonl)   |
| 2026-05-27 | v0.1.0       | from_bytes_heap_64B   | …                 | …                 | …            | (see jsonl)   |

Raw data: `rust-results-2026-05-27.jsonl`. C / C++ / Go cohorts pending —
this is the Rust-only first pass to validate the harness and uncover
the heap-clone gap.

## What needs to happen before kevy-bytes v0.1.0 can publish

1. **Fix bench resolution at sub-ns ops** — `eq` and inline-clone fall
   into noise. Increase ITER (or sample inner-loop wall-clock) and add
   compiler-defeating `black_box` around equal-value inputs so `eq` is
   not const-folded.
2. **Add C / C++ / Go competitor benches** — Rust-only is half the
   picture per the [[feedback-mailrs-stone-deep-polish-method]] "≥ max
   across Rust/Go/C/C++" rule.
3. **Investigate kevy-bytes clone_heap_64B 1.57× gap vs Vec<u8>** —
   the heap clone path has overhead Vec lacks. Likely candidates:
   tag-byte branch before alloc; `Layout::array::<u8>(cap).unwrap()`
   construction; `alloc::alloc` indirection. Read `crates/kevy-bytes/src/lib.rs`
   Clone impl + flamegraph.
4. **Re-bench post-optimisation** — once kevy-bytes ≤ min(owned cohort)
   on every workload, then snapshot as `BASELINE-v0.1.0.md` and publish.

## How to reproduce (Rust cohort)

```bash
cd perfs/comparative/kevy-bytes/rust
cargo build --release
$CARGO_TARGET_DIR/release/kevy-bytes-comparative-bench > ../rust-results-$(date +%F).jsonl
jq -s 'group_by(.workload) | map({workload:.[0].workload, ranked:(sort_by(.value_median) | map({competitor, median:.value_median, p95:.value_p95}))})' \
  ../rust-results-$(date +%F).jsonl
```
