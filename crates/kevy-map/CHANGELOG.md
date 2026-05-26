# Changelog

All notable changes to **kevy-map** will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-26

### Added

- Initial release of `KevyMap<K, V>` — open-addressing Swiss-table-style
  hashtable with SIMD probing on x86_64 + aarch64.
- `bucket_addr_for(&K) -> *const u8` exposing the underlying slot pointer
  for caller-driven prefetch (the key kevy-rt hot-path mechanism for
  hiding DRAM latency at 10M+ keys).
- `KevyMap::prefetch_t0(ptr)` wrapper around `_mm_prefetch` / `prfm` (LE
  64-bit x86_64 + aarch64; semantic no-op elsewhere and under miri).
- Drop-in `insert`/`get`/`remove`/`len`/`is_empty`/`with_capacity`/`iter`.

### Verified

- 99%+ line coverage (`cargo-llvm-cov`); perf gate (`tests/perf_gate.rs`).
- 7/8 max load factor; power-of-two slot count for cheap `& mask` indexing.

### Performance

- Standalone microbench is **0.75–1.00×** of `std::HashMap + FxBuildHasher`
  on mac aarch64 (see [`BUDGETS.md`](./BUDGETS.md) for the honest reading).
  Production wins come from caller-driven prefetch + `SmallBytes` keys, not
  the standalone bench.

[Unreleased]: ./
[0.1.0]: ./
