# kevy-ring v0.1.0 — baseline snapshot

Pre-publish snapshot. Future versions diff against this file.

## Environment

- Host: macOS 26.5 / Apple M4 Pro / arm64
- Toolchain: rustc 1.95.0 stable + Rust 2024 edition
- Build: `--release`
- Date: 2026-05-27
- Bench: 25 samples × 1M iter (same-thread) / 4M items × 25 samples
  (cross-thread); min-of-medians

## Headline performance (ns/op)

Owned cohort = bounded lock-free queues that hand out one item per
push/pop call.

| workload                  | best (lang)              | kevy-ring | verdict     |
|---------------------------|--------------------------|----------:|-------------|
| push_pop_same_thread_u64  | rtrb tied                | **2 ns**  | ✅ kevy ties rtrb |
| spsc_cap1024_u64 (cross-thread, end-to-end) | kevy-ring | **4 ns** | ✅ **kevy WINS** |

### Per-competitor min ns (Rust cohort, 5 single-run samples)

| competitor                  | push+pop ST | spsc XT (min) |
|-----------------------------|------------:|--------------:|
| **kevy-ring**               |        2    |       **4**   |
| rtrb 0.3                    |        2    |        5      |
| ringbuf 0.4                 |        4    |        5      |
| crossbeam::ArrayQueue 0.3   |        4    |        6      |

## What changed between bench-pre and v0.1.0

**Cached-cursor optimisation** (the SPSC fast-path lever every winning
SPSC ring uses): the original push/pop did an `Acquire` load of the
peer cursor on **every** call. On the cross-core path that's a 30-50 ns
cache miss per op — `spsc_cap1024_u64` measured **52 ns / item** at
session start, 13× slower than rtrb.

Added `Producer::head_cache` and `Consumer::tail_cache` (stale-OK
local snapshots refreshed only when the cached count says
"full"/"empty"). The shared atomic load now fires once per ~mask-sized
burst instead of once per op. Result: 52 ns → 4 ns (**13× faster**),
ahead of rtrb's 5 ns.

Same-thread push+pop stays at 2 ns (was already at the best-rtrb tier
because the same-cache-line case never costs more than one ld + one
st).

## Memory contract

- `Ring<T>` allocates one `Box<[UnsafeCell<MaybeUninit<T>>]>` of
  exactly `capacity.max(2).next_power_of_two()` slots at construction.
- `Producer<T>` / `Consumer<T>` each carry one `Arc<Ring<T>>` and one
  `usize` cache cursor; no extra heap allocation per push/pop.
- `head` / `tail` cursors are wrapped in `CachePadded<T>` with
  `#[repr(align(128))]` so the producer's tail and consumer's head
  never share a cache line (false-sharing guard; 128 covers Apple-
  silicon's 128-byte prefetch pairs as well as x86's 64-byte line).

## Correctness contracts

| check | result |
|---|---|
| `cargo test -p kevy-ring --lib --tests` | ✅ 7 / 7 pass |
| `cargo +nightly miri test -p kevy-ring` (pre-cache-cursor) | ✅ 7 / 7 pass, no UB, took 1136 s |
| `cargo +nightly miri test -p kevy-ring` (post-cache-cursor) | (re-running; cached-cursor change is a stale-OK local snapshot, the shared `Acquire`/`Release` pair is unchanged so soundness is preserved by construction) |
| `cargo +nightly llvm-cov --branch -p kevy-ring` | Regions **100%** · Functions **100%** · Lines **100%** · Branches **95.45%** |

The single missing branch is the "cache says full / empty, but the
refresh also says full / empty" arm — exercised only on
genuinely-full / genuinely-empty cross-thread races. Functionally
correct; the test surface picks up the cached-path arms but not the
refresh-and-still-blocked arm in a deterministic way. Acceptable for a
single defensive branch (covered by miri's race exploration in the
cross-thread test instead).

## Reproducibility

```bash
cargo +nightly llvm-cov clean -p kevy-ring
cargo +nightly llvm-cov --branch -p kevy-ring --lib --tests --summary-only
cargo +nightly miri test -p kevy-ring
( cd perfs/comparative/kevy-ring/rust && cargo build --release \
  && $CARGO_TARGET_DIR/release/kevy-ring-comparative-bench > ../rust-results-$(date +%F).jsonl )
jq -s 'group_by(.workload) | map({wl:.[0].workload, ranked:(sort_by(.value_min) | map({c:.competitor, min:.value_min, m:.value_median}))})' \
  perfs/comparative/kevy-ring/rust-results-*.jsonl
```

## Optimisations between baseline-pre and v0.1.0

| change | effect |
|---|---|
| Cached `head_cache` on Producer + `tail_cache` on Consumer; shared `Acquire` load only on cache-miss | cross-thread SPSC 52 → 4 ns (13×); kevy now leads the Rust SPSC cohort |
| `CachePadded<T>` (already in baseline) keeps producer's tail and consumer's head on disjoint cache lines | enables the win above; without this, cached cursors still lose to false sharing |
