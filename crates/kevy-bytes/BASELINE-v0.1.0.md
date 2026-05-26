# kevy-bytes v0.1.0 — baseline snapshot

Pre-publish perf snapshot for the v0.1.0 stone version. Future
versions diff against this file (5-run min-of-medians is the headline
number; full distribution lives in `perfs/comparative/kevy-bytes/`).

## Environment

- Host: macOS 26.5 / Apple M4 Pro / arm64
- Toolchain: rustc 1.95.0 (stable) + Rust 2024 edition
- Build: `--release` (fat LTO + codegen-units 1 + panic=abort, see
  workspace `[profile.release]`)
- Date: 2026-05-27
- Git sha at snapshot: see commit hash on this file

## Headline performance (5-run min-of-medians, ns/op)

Owned-cohort cross-language gate. Best = `min(median)`; lower is better.

| workload          | best (lang)              | kevy-bytes | verdict                |
|-------------------|--------------------------|-----------:|------------------------|
| clone_inline_12B  | 0 (kevy / smartstring / compact_str) | 0 | **kevy tied for win**  |
| clone_heap_64B    | 15 (Go []byte)           | 23         | 8 ns behind allocator-tier |
| eq_inline_12B     | ≤ 2 (kevy)               | ≤ 2        | **kevy ties / wins**   |
| eq_heap_64B       | 3 (kevy / Vec / std::String) | 3      | **kevy tied for win**  |
| from_bytes_inline | 3 (kevy)                 | 3          | **kevy wins outright** |
| from_bytes_heap   | 17 (sds)                 | 25         | 8 ns behind allocator-tier |

### Workloads kevy-bytes WINS or TIES (all SSO-inline + all eq)

These are the workloads kevy-bytes was designed to optimise — short
byte strings (≤ 22 bytes inline, the kevy keyspace common case).

### Workloads kevy-bytes LOSES by 6-8 ns

`clone_heap_64B` and `from_bytes_heap_64B` — 64-byte heap allocation
hot paths. The gap is **allocator-tier**: Go's runtime maintains a
per-P size-class pool that serves 64-byte allocations in ~15 ns; macOS
libmalloc (Rust's default on Darwin) is at ~20-25 ns for the same op.
This 8 ns is structural to the platform allocator; no code-level
change in kevy-bytes can close it without switching to mimalloc /
jemalloc (which would violate the project's 0-dep charter at
publish-crate level).

## Per-workload absolute numbers

| competitor (cohort)      | clone_in | clone_he | eq_in | eq_he | from_in | from_he |
|--------------------------|---------:|---------:|------:|------:|--------:|--------:|
| **kevy-bytes**           |       0  |      23  |   ≤2  |    3  |      3  |     25  |
| Vec<u8> (Rust)           |      16  |      26  |    1  |    3  |     —   |     —   |
| std::String (Rust)       |      17  |      17  |    1  |    3  |     —   |     —   |
| smartstring (Rust)       |       0  |      22  |    —  |    3  |     —   |     —   |
| compact_str (Rust)       |       0  |      28  |    —  |    4  |     —   |     —   |
| smol_str (Rust, shared)  |       0  |       5* |    —  |    —  |     —   |     —   |
| std::string (C++ libc++) |       1  |      20  |    2  |    3  |      5  |     18  |
| sds (C, antirez/sds)     |      17  |      17  |    4  |    4  |     16  |     17  |
| []byte (Go)              |       2  |      15  |   36† |   37† |     16  |     19  |
| string (Go, shared)      |       1* |       1* |   35† |   36† |      3  |     16  |

\* smol_str / Go string clone is **reference-count increment**, not a
copy — semantically different from owned-byte-string clone; excluded
from gate.

† Go []byte eq via `bytes.Equal` runs ~37 ns on this host; the inline
function-call overhead of the Go bench harness dominates a 64-byte
memcmp at this scale.

## Memory contract

- `size_of::<SmallBytes>() == 24` — const-asserted at compile time
  (`const _: () = { assert!(...) };` in src/lib.rs).
- `align_of::<SmallBytes>() == align_of::<usize>()` — same.
- Inline variant: zero heap allocations, value lives entirely in the
  24-byte struct.
- Heap variant: one allocation of exactly `len` bytes (no over-
  allocation; capacity field equals length on first construction).
- Drop: deallocates the heap buffer exactly once via `dealloc(ptr,
  Layout::array::<u8>(cap))`.

Per-op heap allocation: **0** for inline (`len ≤ 22`) — the kevy
keyspace common case. **1** alloc + **1** memcpy for heap (`len ≥ 23`).

## Correctness contracts

- **`cargo test -p kevy-bytes --lib --tests`**: 30 / 30 pass.
- **`cargo +nightly miri test -p kevy-bytes`**: 30 / 30 pass, no UB.
- **`cargo +nightly llvm-cov --branch -p kevy-bytes`** (clean run):
  - Regions: 99.09%
  - Functions: 100%
  - Lines: 98.74%
  - Branches: 88.89%

The 2 uncovered branches are the defensive `unreachable!` in the
specialised `PartialEq` (which can only be reached by violating
construction invariants — see source comment) and the `cfg(miri)` /
arch-specific branch in `prefetch_t0` (the inline-asm `prfm` /
`_mm_prefetch` paths cannot run under miri).

## Reproducibility

```bash
# Effective coverage
cargo +nightly llvm-cov clean -p kevy-bytes
cargo +nightly llvm-cov --branch -p kevy-bytes --lib --tests --summary-only

# Miri (UB-free)
cargo +nightly miri test -p kevy-bytes

# Cross-language bench (Rust)
cd perfs/comparative/kevy-bytes/rust
cargo build --release
for i in 1 2 3 4 5; do
  $CARGO_TARGET_DIR/release/kevy-bytes-comparative-bench
done > rust-multirun.jsonl
jq -s 'group_by([.competitor, .workload]) | map({c:.[0].competitor, w:.[0].workload, min:([.[].value_median] | min)})' rust-multirun.jsonl

# Cross-language bench (C++ / C / Go)
( cd perfs/comparative/kevy-bytes/cpp && make && ./bench )
( cd perfs/comparative/kevy-bytes/c   && make && ./bench )
( cd perfs/comparative/kevy-bytes/go  && go build -o bench ./... && ./bench )
```

## Optimisations between baseline-pre and v0.1.0

(See git log + perfs/comparative/kevy-bytes/P1-STATUS.md for detail.)

1. Specialised `Clone` — inline = union bitwise copy, heap = direct
   alloc + memcpy, skips `as_slice → from_slice` chain.
2. Specialised `PartialEq` — single tag-byte dispatch (inline/inline,
   heap/heap, defensive `unreachable!` for mixed which the public API
   cannot construct).
3. `alloc_heap` + `clone_heap` use `Layout::from_size_align_unchecked
   (len, 1)` — drops the unreachable panic check from
   `Layout::array::<u8>(len).expect(...)`.
4. Added 13 effective-coverage tests (no padding — each asserts a
   specific contract: Hash/KevyHash agreement with `&[u8]`, Borrow
   round-trip through HashMap, Drop on inline is no-op, clone produces
   independent heap buffer, partial_cmp matches cmp, etc.).
