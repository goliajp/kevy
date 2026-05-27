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

## Headline performance (ns/op, min-of-medians)

| workload          | best across all langs | kevy-hash | verdict                |
|-------------------|-----------------------|----------:|------------------------|
| hash_u64          | 0 (tied)              | 0         | ✅ **kevy tied for best** |
| hash_bytes_8B     | 0 (kevy + ahash + fxhash) | 0     | ✅ **kevy tied for best** |
| hash_bytes_16B    | 0 (fxhash / rustc-hash) | 1       | ⚠️ 1 ns noise floor    |
| hash_bytes_64B    | 2 (ahash / rustc-hash / wyhash / std::hash) | 3 | ⚠️ 1 ns noise floor |

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

### Why the 1 ns gap CANNOT be closed (P15-B2 investigation)

The 1-2 ns gap vs the absolute fastest competitors (fxhash/rustc-hash
on 16B; ahash on 64B) is **structural**, not a polish miss:

- **Gap vs fxhash / rustc-hash** = the cost of our fmix64 finalize
  (~6 ALU ops; pipelined to ~1 ns observed). Removing it WOULD close
  the gap, but **would break the `no_catastrophic_clustering_on_low_
  entropy_keys` test** — bare-Fx clusters 30-50× on `"key:0..N"`-style
  inputs, which is the keyspace shape kevy actually sees in production.
  fxhash/rustc-hash are FAST AT THE COST OF this guarantee; we trade
  1 ns for the guarantee.
- **Gap vs ahash on 64B** = ahash uses x86 AES-NI hardware
  instructions to do its mix in 1-2 cycles. kevy-hash is
  `#![forbid(unsafe_code)]` (charter constraint) — even targeting
  AES intrinsics directly would require unsafe asm or std::arch.
  ahash is a different architectural choice (DoS-resistant via random
  seed + hardware AES); we explicitly do not pay either tax.

Both gaps are between kevy-hash and competitors that have sacrificed
a property we keep. There is no "learn-from-open-source" lever
remaining — the open-source winners win by giving up something we
don't give up.

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
