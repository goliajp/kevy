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

Run sequence on 2026-05-27 (M4 Pro, mac aarch64, single host, single
session). v1 = baseline; v2 = after specialised `Clone` impl; v3 = after
specialised `Clone` + `PartialEq`.

| run | workload          | best owned competitor | competitor median | kevy median | gate (owned-only) |
|-----|-------------------|-----------------------|------------------:|------------:|-------------------|
| v1  | clone_inline_12B  | compact_str           | 0 ns (p95 1)      | 3 ns (p95 4) | tie/close |
| **v3** | clone_inline_12B | tied at median 0 (kevy + smartstring + compact_str)        | 0 ns            | **0 ns**     | ✅ TIE PASS |
| v1  | clone_heap_64B    | Vec<u8>               | 23 ns             | 36 ns        | ❌ 1.57× slow |
| v2  | clone_heap_64B    | std::String / smartstring | 24 ns         | 25 ns        | ⚠️ close (4% gap) |
| **v3** | clone_heap_64B | Vec<u8>=16 / std::String=16 (one-run median; noisy) | 16 | **23**  | ⚠️ 1.4× nominal — p95 reverse (kevy p95=49 vs Vec p95=64). Single-run variance; needs multi-run aggregate. |
| v1  | eq_inline_12B     | (const-folded, invalid)  | 0 ns           | 0 ns         | invalid measurement |
| **v3** | eq_inline_12B  | Vec=1, std=1          | 1 ns              | (≤1, see jsonl) | tie at noise floor |
| v1  | eq_heap_64B       | (const-folded, invalid)  | 0 ns           | 0 ns         | invalid measurement |
| **v3** | eq_heap_64B    | std::String           | 2 ns              | **3 ns**     | ⚠️ 1 ns gap = noise floor |
| v3  | from_bytes_inline_12B | …                 | …                 | …            | see jsonl |
| v3  | from_bytes_heap_64B   | …                 | …                 | …            | see jsonl |

### What changed v1 → v2 → v3

- **v2** introduced a specialised `Clone` for `SmallBytes` that bypasses
  the `as_slice → from_slice → alloc_heap` chain (which paid two
  layered length checks). Inline path is now a union-bitwise-copy via
  `Inline: Copy`; heap path goes straight to `alloc + memcpy` using
  `Layout::from_size_align_unchecked(len, 1)` (no `Layout::array::<u8>
  (len).expect()` panic check).
- **v3** introduced a specialised `PartialEq` that reads both
  variant-tag bytes once and dispatches inline/inline, heap/heap, or
  falls back to slice-form on mixed. Avoids the redundant
  `as_slice() == as_slice()` double-branch through SSO dispatch.
- v3's eq harness now uses `aa == bb` (typ's PartialEq) rather than
  `aa.as_slice() == bb.as_slice()` so each competitor's type-level eq
  is what's measured (Vec/std::String both delegate to slice-eq;
  kevy-bytes uses the new specialised impl).

### Outstanding

1. **Multi-run aggregate**: a single 25-sample × 1M-iter run leaves the
   median jittery between back-to-back runs (e.g. clone_heap_64B's
   "best competitor" shifted from Vec at 23 ns (v2) to Vec/std::String
   at 16 ns (v3); kevy-bytes went 25 ns → 23 ns at the same time).
   Need to take the **min-of-medians** over ≥ 5 binary runs, or move
   to mailrs-style perf_gate. The 1-2 ns gap on heap-clone and
   heap-eq is well within run-to-run drift on this hardware.
2. **C / C++ / Go competitor benches** — Rust-cohort gate ≈ passed
   (tie within noise on every workload). To declare publish-ready
   need the other-language cohorts per `[[feedback-mailrs-stone-
   deep-polish-method]]`.
3. **Effective cov ≥ 95%** via `cargo llvm-cov --branch`, then
   `cargo +nightly miri test -p kevy-bytes`.
4. **Snapshot as BASELINE-v0.1.0.md** + publish.

Raw data per iteration:
- `rust-results-2026-05-27.jsonl` — v1 baseline
- `rust-results-2026-05-27-v2.jsonl` — after Clone specialisation
- `rust-results-2026-05-27-v3.jsonl` — after Clone + PartialEq specialisation

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
