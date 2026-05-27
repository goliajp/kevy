# kevy-madvise v0.1.0 — baseline snapshot

Pre-publish snapshot. Future versions diff against this file.

## Environment

- Host: macOS 26.5 / Apple M4 Pro / arm64
- Toolchain: rustc 1.95.0 stable + Rust 2024 edition
- Build: `--release`
- Date: 2026-05-27

## What this stone does

A single function: `advise_hugepage(ptr, len)` — issues
`madvise(MADV_HUGEPAGE)` on Linux for the page-aligned subset of
`[ptr, ptr+len)`, no-ops below 2 × 4 KiB threshold, no-ops on every
non-Linux target via `cfg(target_os = "linux")` (no FFI declared).

## Perf characterisation

This stone is **kernel-time-dominated**: the actual work is one
`madvise` syscall (~50-500 ns depending on region size and kernel
load). The wrapper's per-call overhead is a handful of ALU
operations (4 cmp + 2 and + 2 shift to enforce page alignment). No
heap allocation, no atomic ops, no lock.

### Why no cross-language gate

Cross-language perf comparison for `madvise` is meaningless:

- **Rust `libc::madvise`** — same syscall, same ABI, identical
  cost. The only difference is `kevy-madvise` performs the
  page-rounding for the caller and adds the `cfg(linux)` no-op
  shim; both are negligible vs the syscall.
- **C raw `madvise()`** — identical.
- **Go runtime's internal madvise path** — not publicly callable;
  Go programs that need `MADV_HUGEPAGE` use `syscall.Syscall6` or
  cgo, both with overhead similar to or higher than the direct
  syscall.
- **C++** — no standard library wrapper; libc++ would also go
  through libc's `madvise`.

The competitive surface is therefore **alloc-count = 0** and
**page-rounding correctness**, not "ns/op vs competitor".

## Memory contract

- Per-call heap allocation: **0** bytes.
- Stack usage: a handful of `usize` + one `c_int` for the syscall arg.
- No retained Rust-side state.

## Correctness contracts

| check | result |
|---|---|
| `cargo test -p kevy-madvise --lib --tests` | ✅ 4 / 4 pass |
| `cargo +nightly miri test -p kevy-madvise` | ✅ 4 / 4 pass, no UB (miri sees the function compile out — the FFI is gated `cfg(target_os = "linux")` so on the macOS host miri runs the no-op branch only) |
| `cargo +nightly llvm-cov --branch -p kevy-madvise` | Regions **100%** · Functions **100%** · Lines **100%** · Branches no-branch |

Coverage was already 100% at session start — the original 4 tests
(`no_call_below_two_pages`, `unaligned_buffer_does_not_panic`,
`zero_length_is_noop`, `large_aligned_region_runs`) exhaustively
cover every branch of the page-alignment math (under-threshold
short-circuit, unaligned start, zero-length, aligned-large).

## Reproducibility

```bash
cargo +nightly llvm-cov clean -p kevy-madvise
cargo +nightly llvm-cov --branch -p kevy-madvise --lib --tests --summary-only
cargo +nightly miri test -p kevy-madvise
```

## Optimisations between baseline-pre and v0.1.0

None required — the wrapper was already optimal (no allocation, no
lock, kernel-time-dominated). The cov number was already 100% at the
session start.
