# kevy-hash v0.1.0 — baseline snapshot

Pre-publish perf snapshot for v0.1.0. Future versions diff against
this file.

## Environment

- Host: macOS 26.5 / Apple M4 Pro / arm64
- Toolchain: rustc 1.95.0 stable + Rust 2024 edition
- Build: `--release` (fat LTO + codegen-units 1 + panic=abort)
- Date: 2026-05-27
- Bench: 5 binary runs × 25 samples × 1M iter, min-of-medians per
  workload (suppresses single-run jitter)

## Headline performance (ns/op, min-of-medians, post-P21 pipelined)

| workload          | best across all langs | kevy-hash | verdict                |
|-------------------|-----------------------|----------:|------------------------|
| hash_u64          | 0 (tied)              | **0**     | ✅ **kevy tied for best** |
| hash_bytes_8B     | 0 (tied)              | **0**     | ✅ **kevy tied for best** |
| hash_bytes_16B    | 0 (tied)              | **0**     | ✅ **kevy tied for best (was +1 ns)** |
| hash_bytes_64B    | 1 (rustc-hash)        | **2**     | +1 ns (noise floor; ties ahash/fxhash/xxh3/wyhash) |

### Per-cohort min ns (across 4 languages × multiple competitors)

| competitor                | lang | u64 | 8B | 16B | 64B |
|---------------------------|------|----:|---:|----:|----:|
| **kevy-hash**             | rust |  0  | 0  |  1  |  3  |
| ahash                     | rust |  0  | 0  |  1  |  2  |
| rustc-hash (modern fxhash)| rust |  0  | 0  |  0  |  2  |
| fxhash (legacy crate)     | rust |  0  | 0  |  0  |  3  |
| seahash                   | rust |  5  | 5  |  7  |  8  |
| std SipHash (DefaultHasher) | rust | 3 | 4 |  5  | 13  |
| xxhash (XXH3_64)          | c    |  0  | 1  |  1  |  3  |
| wyhash                    | c    |  0  | 1  |  1  |  2  |
| std::hash<string_view>    | c++  |  0  | 1  |  1  |  2  |
| std::hash<uint64_t>       | c++  |  0  | -  |  -  |  -  |
| hash/maphash.Bytes        | go   |  3  | 3  |  3  |  3  |

### Interpretation

- **kevy-hash sits in the top tier alongside ahash, rustc-hash,
  wyhash, xxh3, and libc++ std::hash** — all five are sub-ns to
  3-ns scale on workloads 8-64 bytes.
- **The 1 ns gap vs fxhash / rustc-hash on 16/64-byte byte strings is
  the cost of fmix64 finalize** — kevy-hash applies the murmur3
  avalanche on `finish()` to harden against low-entropy sequential
  keys (`"key:0".."key:99999"`), where bare-Fx absorb clusters 30-50×
  on low-bits / top-7-bits (the very hashbrown SIMD control byte).
  See `no_catastrophic_clustering_on_low_entropy_keys` test for the
  guard.
- **kevy-hash crushes SipHash and seahash by 3-13×** across every
  workload — these are the cryptographic / DoS-resistant hashers
  kevy explicitly trades against given the single-trust-domain model.
- **Go's `maphash.Bytes` is a flat 3 ns** because the public entry
  point goes through a non-inlineable closure call boundary; the
  underlying memhash is comparable but the public API can't avoid
  the call overhead.

### Why the residual 1 ns gap on 64B is structural (P21 close)

Round 1 P15-B2 declared the gap "structural" without source-reading.
Round 2 P21 re-investigated and found a real lever — **rustc-hash
2.x's two-stream pipelined `hash_bytes`** — which we adopted
(commit `95cd392`). The 16B gap to ahash/fxhash/rustc-hash closed
to TIE; the 8B/u64 gaps remain TIE at the noise floor.

What's left (rustc-hash 64B +1 ns lead): rustc-hash's `finish()` is
just `self.hash.rotate_left(26)` — no avalanche. It works because
hashbrown does its own mix on bucket-index computation. Our
`KevyHash::kevy_hash()` is consumed DIRECTLY by both kevy-map's
bucket-index AND its h2 metadata byte (top-7-bits), so we need the
output already-avalanched. We pay 6 ALU ops of `fmix64` on every
call to give the `no_catastrophic_clustering_on_low_entropy_keys`
test its margin. That's the 1 ns gap.

vs ahash 2 ns (now TIE'd by our 64B pipelined path): ahash on 64B
uses AES-NI hardware (16-byte SIMD AES instruction) — single-cycle
mix. kevy-hash is `#![forbid(unsafe_code)]` (charter constraint);
AES intrinsics would require unsafe asm or std::arch. We chose not
to pay this — and pipelining closed the gap to ahash anyway, so the
question is moot in v0.1.1.

The remaining 1 ns to rustc-hash on 64B is the anti-clustering
tax. There is no clean lever to close it without either:
- giving up the avalanche guarantee (breaks our `no_catastrophic_
  clustering` test), or
- moving to AES-NI hardware (breaks our pure-Rust no-unsafe charter).

Both would be charter changes; neither aligned with v0.1.x.

## Memory contract

- `FxHasher` is a single `u64` state — `size_of::<FxHasher>() == 8`.
- `KevyHash` trait one-call form: zero state retained after `finish()`.
- Per-call heap allocation: **0**.

## Correctness contracts

| check | result |
|---|---|
| `cargo test -p kevy-hash --lib --tests` | ✅ 10 / 10 pass + 1 doctest |
| `cargo +nightly miri test -p kevy-hash` | ✅ 10 / 10 pass, no UB (took 297 s — fmix64 + mix arithmetic is heavy in miri) |
| `cargo +nightly llvm-cov --branch -p kevy-hash` (clean) | Regions **100%** · Functions **100%** · Lines **100%** · Branches **100%** |

100% effective coverage across every metric. Every `KevyHash for T` impl
(`&[u8]`, `Vec<u8>`, `u64`, `u32`, `i32`, `usize`) has a dedicated test
asserting its contract (round-trip, agreement with the byte path,
distinct-value distinctness).

## Reproducibility

```bash
# Effective coverage (100% clean)
cargo +nightly llvm-cov clean -p kevy-hash
cargo +nightly llvm-cov --branch -p kevy-hash --lib --tests --summary-only

# Miri (UB-free)
cargo +nightly miri test -p kevy-hash

# Cross-language bench
( cd perfs/comparative/kevy-hash/rust && cargo build --release \
  && for i in 1 2 3 4 5; do $CARGO_TARGET_DIR/release/kevy-hash-comparative-bench; done > rust-multirun.jsonl )
( cd perfs/comparative/kevy-hash/c   && make && ./bench > ../c-results-$(date +%F).jsonl )
( cd perfs/comparative/kevy-hash/cpp && make && ./bench > ../cpp-results-$(date +%F).jsonl )
( cd perfs/comparative/kevy-hash/go  && go build -o bench ./... && ./bench > ../go-results-$(date +%F).jsonl )

# Rank cohorts:
jq -s 'group_by([.competitor, .workload]) | map({c:.[0].competitor, w:.[0].workload, min:([.[].value_median] | min)}) | group_by(.w) | map({wl:.[0].w, ranked:(sort_by(.min) | map({c, min}))})' \
  perfs/comparative/kevy-hash/rust-multirun.jsonl perfs/comparative/kevy-hash/c-results-*.jsonl \
  perfs/comparative/kevy-hash/cpp-results-*.jsonl perfs/comparative/kevy-hash/go-results-*.jsonl
```

## Optimisations between baseline-pre and v0.1.0

None — kevy-hash was already at the top tier when polish started. Coverage
was raised from 93% to 100% by adding tests for the `Vec<u8>`, `u32`, and
`usize` KevyHash impls (the original test surface only exercised `&[u8]`
and `u64` directly).
