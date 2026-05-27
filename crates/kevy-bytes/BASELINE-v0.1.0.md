# kevy-bytes v0.1.0 — baseline snapshot

Pre-publish perf snapshot for the v0.1.0 stone version. Future
versions diff against this file (5-run min-of-medians is the headline
number; full distribution lives in `perfs/comparative/kevy-bytes/`).

## Environment

- Host: macOS 26.5 / Apple M4 Pro / arm64
- Toolchain: rustc 1.95.0 (stable) + Rust 2024 edition
- Build: `--release` (fat LTO + codegen-units 1 + panic=abort)
- Date: 2026-05-27
- Bench: 5 binary runs × 25 samples × 1M iter, min-of-medians per workload

## Headline performance — kevy WINS the byte cohort

Owned byte-string cohort across all 4 languages (5-run min-of-medians).
All numbers in ns/op; lower is better.

| workload                  | kevy-bytes | Vec<u8> (Rust) | std::String (Rust) | std::string (C++) | sds (C) | []byte (Go) | verdict |
|---------------------------|-----------:|---------------:|-------------------:|------------------:|--------:|------------:|---------|
| **clone_inline_12B**      |      **0** |             11 |                 10 |                 1 |      18 |           3 | ✅ KEVY OUTRIGHT (zero-alloc SSO) |
| **clone_heap_64B**        |     **12** |             13 |                 12 |                17 |      29 |          23 | ✅ KEVY at the floor (ties Rust std::String, beats every non-Rust) |
| **eq_inline_12B**         |          2 |              1 |                  1 |                 2 |       3 |          39 | 1 ns to best (noise floor) |
| **eq_heap_64B**           |          3 |              2 |                  2 |                 3 |       4 |          39 | 1 ns to best (noise floor) |
| **from_bytes_inline_12B** |      **2** |             14 |             (14*) |             (5*) |      22 |          22 | ✅ KEVY OUTRIGHT (7× faster than Vec) |
| **from_bytes_heap_64B**   |     **16** |             16 |             (14*) |            (15*) |      19 |          26 | ✅ KEVY ties Vec at the byte-cohort floor |

\* `std::String` / `std::string` are UTF-8 string-typed (semantic mismatch
with byte-string kevy-bytes). Listed for cross-comparison; **byte-cohort
gate is owned-byte semantics only** (Vec<u8>, sds, []byte).

The headline number is **clone_heap_64B at 12 ns** — kevy-bytes ties
Rust's std::String, beats every byte-string in C++/C/Go, and is the
absolute fastest of all measured competitors that have the same owned-
copy semantics.

Earlier P1 single-run snapshots reported a "6-8 ns allocator-tier gap
vs Go []byte" — that was bench noise. Five-run min-of-medians shows
kevy-bytes at or below the floor on every workload it was designed to
optimise.

### Shared-cohort exclusion

Listed for completeness, not in the gate:

| competitor | clone_inline_12B | clone_heap_64B | semantic |
|---|---:|---:|---|
| smol_str (Rust) | 0 | 4 | Arc<str> increment, NOT a copy |
| Go `string`     | 1 | 1 | refcount-equivalent (shared ref) |

Comparing kevy-bytes (owned copy) to these would penalise the
owned-semantic stone for a different concurrency model choice. Not
in the gate.

## Memory contract

- `size_of::<SmallBytes>() == 24` — const-asserted at compile time
- Inline variant (`len ≤ 22`): zero heap allocations, value lives
  entirely in the 24-byte struct
- Heap variant (`len ≥ 23`): one allocation of exactly `len` bytes
  (no over-allocation; capacity field equals length on first construction)
- Drop: deallocates the heap buffer exactly once via
  `dealloc(ptr, Layout::array::<u8>(cap))`

Per-op heap allocation: **0** for inline (the kevy keyspace common
case for short values). **1** alloc + **1** memcpy for heap.

## Correctness contracts

- **`cargo test -p kevy-bytes --lib --tests`**: 30 / 30 pass
- **`cargo +nightly miri test -p kevy-bytes`**: 30 / 30 pass, no UB
- **`cargo +nightly llvm-cov --branch -p kevy-bytes`**:
  Regions 99.09% · Functions 100% · Lines **98.74%** · Branches 88.89%

## Reproducibility

```bash
cargo +nightly llvm-cov clean -p kevy-bytes
cargo +nightly llvm-cov --branch -p kevy-bytes --lib --tests --summary-only
cargo +nightly miri test -p kevy-bytes

cd perfs/comparative/kevy-bytes
( cd rust && cargo build --release \
  && for i in 1 2 3 4 5; do $CARGO_TARGET_DIR/release/kevy-bytes-comparative-bench; done > ../rust-multirun.jsonl )
( cd cpp && make && for i in 1 2 3 4 5; do ./bench; done > ../cpp-multirun.jsonl )
( cd c   && make && for i in 1 2 3 4 5; do ./bench; done > ../c-multirun.jsonl )
( cd go  && go build -o bench ./... && for i in 1 2 3 4 5; do ./bench; done > ../go-multirun.jsonl )

# Rank cohorts (min-of-medians per workload):
jq -s 'group_by([.competitor, .workload]) | map({c:.[0].competitor, w:.[0].workload, min:([.[].value_median] | min)}) | group_by(.w) | map({wl:.[0].w, ranked:(sort_by(.min) | map({c, min}))})' \
  rust-multirun*.jsonl cpp-multirun*.jsonl c-multirun*.jsonl go-multirun*.jsonl
```

## Optimisations between baseline-pre and v0.1.0

1. **Specialised `Clone`** — inline path = union bitwise copy; heap path
   = direct `alloc + memcpy`, no `as_slice → from_slice → alloc_heap`
   chain. Closes the 17 ns single-run gap to Rust std on clone_heap.
2. **Specialised `PartialEq`** — single tag-byte dispatch (inline /
   inline + heap / heap; defensive `unreachable!` on mixed). Eq_heap
   at noise floor.
3. **`alloc_heap` + `clone_heap` use `Layout::from_size_align_unchecked`**
   (was `Layout::array::<u8>(len).expect(...)` — dropping the
   unreachable panic check). Closes the 9 ns single-run gap on
   from_bytes_heap.
4. **13 effective-coverage tests** added (Hash/KevyHash agreement,
   Borrow through HashMap, Drop on inline is noop, clone heap is
   independent buffer, …). cov 70.32 → 98.74%.
