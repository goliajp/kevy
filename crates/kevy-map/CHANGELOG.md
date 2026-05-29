# Changelog

All notable changes to **kevy-map** will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-26

### Added

- Initial release of `KevyMap<K, V>` — open-addressing Swiss-table-style
  hashtable with SIMD probing on x86_64 + aarch64.
- `KevyMap::prefetch_for_hash(hash)` — hint the next bucket cache line
  into L1 via `prefetcht0` (x86_64) / `prfm pldl1keep` (aarch64); the key
  kevy-rt hot-path mechanism for hiding DRAM latency at 10M+ keys.
- Drop-in `insert`/`get`/`get_mut`/`remove`/`len`/`is_empty`/`with_capacity`/`iter`.
- `KevyHash` trait bound (re-exported via `kevy-hash`).

### Verified

- 99%+ line coverage (`cargo-llvm-cov`); perf gate (`tests/perf_gate.rs`).
- 7/8 max load factor; power-of-two slot count for cheap `& mask` indexing.

### Performance

- Standalone microbench is **0.75–1.00×** of `std::HashMap + FxBuildHasher`
  on mac aarch64 (loaded-host reading).
  Production wins come from caller-driven prefetch + `SmallBytes` keys, not
  the standalone bench.

[Unreleased]: ./
[0.1.0]: ./
