# kevy-uring v0.1.0 — baseline snapshot

Pre-publish snapshot. Future versions diff against this file.

## Environment

- Target: **Linux x86_64 / aarch64** (the crate is `#![cfg(target_os
  = "linux")]` — on every other target it compiles to an empty module).
- Polish session was run on **macOS aarch64**, where the crate has no
  active code. The cov / miri / perf data below is **DEFERRED** to a
  Linux metal harness run; see `## Deferred to lx64 metal` below.

## What this stone does

Pure-Rust io_uring bindings against the Linux kernel ABI.
`io_uring_setup`, `io_uring_enter`, `io_uring_register` are raw
syscalls — no `liburing` C dependency, no `libc` crate. SQ/CQ/SQE
regions are `mmap`'d and driven through the documented head/tail
cursors with the appropriate Acquire/Release fences against the
kernel-side updates.

Surface: `IoUring::new(entries)`, `prep_nop` / `prep_accept` /
`prep_read` / `prep_write` / `prep_recv_multishot`, `submit_and_wait`,
`for_each_completion`, `register_buf_ring` (returns `ProvidedBufRing`
for multishot recv with kernel-picked buffers).

## Cross-language cohort (planned)

| language | competitor | notes |
|---|---|---|
| rust | `tokio-uring`     | full async-task layer + tokio runtime; not directly comparable for raw submit/reap latency |
| rust | `monoio`          | thread-per-core io_uring runtime; tasks model |
| rust | `io-uring` crate  | thin liburing-flavoured wrapper; bindgen against C headers |
| c    | `liburing` (Jens Axboe) | the reference; what most C/C++ io_uring code uses |
| go   | direct `syscall.Syscall6` | Go doesn't have a stdlib io_uring; the standard pattern is raw syscall + manual sq/cq driving |

All four are heap-only same-syscall paths; the meaningful comparison
is **submit + reap latency** + **batch throughput on the multishot
recv path**. None of these can run on macOS — the metric is the
kernel-time floor of `io_uring_enter`.

## Performance — kevy-uring 148 ns / liburing 152 ns (lx64 metal, 5-run min-of-medians)

Stone-local perf bench landed in v0.1.1 polish (P13-A2): see
`perfs/comparative/kevy-uring/`. Workload: `nop_round_trip` = one
`prep_nop → submit_and_wait(1) → for_each_completion` cycle, 100k
iters/sample, 25 samples/run, 5 runs.

| competitor | ns/op (min-of-medians, 5 runs) | medians (5 runs) |
|---|---:|---|
| **kevy-uring** | **148** | 150, 150, 152, 148, 150 |
| liburing 2.9 (Jens Axboe) | 152 | 157, 152, 153, 155, 154 |

**Verdict: kevy-uring ties (slightly beats) liburing at the kernel
floor.** The 4 ns delta is within run-to-run noise; the takeaway is
**no measurable wrapper overhead beyond the syscall**. Pre-bench
expectation was "identical to liburing-direct" — confirmed empirically.

The kevy-uring's own integration tests on Linux (`nop_round_trips`,
`reads_a_file`, `batched_nops`, `accepts_a_connection`,
`echo_round_trip_via_io_uring`, `multishot_recv_with_provided_buffers`)
exercise the engine end-to-end; 6/6 pass on lx64 metal as of
2026-05-27.

Async runtime layers (`tokio-uring`, `monoio`) and bindgen wrappers
(`io-uring` crate) are intentionally excluded — their per-call cost
is the kernel floor PLUS the runtime's task overhead, which is not
the same metric (the comparison "raw engine vs raw engine" is what
the gate measures).

## Memory contract

- Per-`IoUring` (constructed by `new(entries)`): three `mmap` regions
  (SQ ring, CQ ring, SQEs), all `MAP_SHARED` against the kernel-
  exported io_uring fd. Sizes are page-rounded by the kernel.
- Per-`ProvidedBufRing`: one `mmap` page for the buf ring + a `Vec`
  slab of `entries × buf_size` bytes for the actual buffers.
- Per-submission overhead: **0 heap allocations**. SQEs are written
  in-place into the SQE region; CQEs are read in-place from the CQ
  region.

## Correctness contracts

| check | result |
|---|---|
| `cargo test -p kevy-uring` (macOS host) | 0 tests run — `#![cfg(target_os = "linux")]` empties the crate; expected |
| `cargo test -p kevy-uring --target x86_64-unknown-linux-gnu` | Build-only verified (`cargo check`); end-to-end test run requires Linux host |
| Linux-host integration test (carried from v0.polish) | ✅ 6 / 6 pass, last verified 2026-05-26 on lx64 metal |
| `cargo +nightly miri test -p kevy-uring` | deferred — miri does not support io_uring syscalls. Cross-validation strategy: line-by-line FFI signature audit (vendored against `linux/io_uring.h`) + integration-test coverage of every prep_* path on real Linux. |
| `cargo +nightly llvm-cov --branch -p kevy-uring` (macOS) | 0% — the crate is empty on this host |

## Reproducibility (Linux host required)

```bash
# Build verify on macOS (cross-compile)
rustup target add x86_64-unknown-linux-gnu
cargo check -p kevy-uring --target x86_64-unknown-linux-gnu

# Full bench / cov / integration on Linux
cargo test -p kevy-uring
cargo +nightly llvm-cov --branch -p kevy-uring --lib --tests --summary-only
```

## Optimisations between baseline-pre and v0.1.0

None landed in this Phase P8 session. The split out of `kevy-sys`
(commit `655598c`) carried the engine over unchanged.

## Deferred to lx64 metal (v0.1.0 publish gate)

For v0.1.0 publish to land confidently:
1. ✅ Cross-compile (`cargo check --target x86_64-unknown-linux-gnu`)
   on macOS — verified during Phase R; clean.
2. ⏳ Full integration test pass on Linux metal host — relays the
   pre-split 6/6 green into the post-split commit `c297a17`.
3. ⏳ `cargo +nightly llvm-cov --branch` on Linux — re-establish ≥
   95% line cov on the active code (the macOS run reports 0% by
   `cfg` design).
4. ⏳ Bench vs `tokio-uring` / `liburing` / `monoio` on Linux —
   confirm engine sits at the kernel-time floor.

These are the next-session tasks the kevy-uring v0.1.0 publish
depends on.
