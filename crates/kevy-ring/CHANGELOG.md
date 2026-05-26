# Changelog

All notable changes to **kevy-ring** will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-26

### Added

- Initial release. `ring::<T>(cap) -> (Producer<T>, Consumer<T>)` — lock-free
  bounded SPSC ring with cache-line-padded head/tail (avoids false sharing
  on the cross-core hop).
- Capacity is rounded **up to a power of two**; index is `& mask` on the
  hot path (no `%`).
- `push` returns `Err(val)` on full so the caller picks the back-off policy.
- `pop -> Option<T>`.

### Memory ordering

- Producer's `tail` store: `Release`.
- Consumer's `head` store: `Release`.
- Cross-side reads: `Acquire`. Same-side reads of own index: `Relaxed`.

### Performance

- Same-thread push+pop ≈ 1 ns/op.
- Cross-thread SPSC on lx64 (x86_64): 6-10 ns/item (104-165M items/sec
  depending on capacity). Mac aarch64 ~63 ns/item due to coherence cost.

### Verified

- 99.11% line coverage.
- 7/7 cross-thread tests pass under `cargo miri test` on `nightly-2026-05-19`.
- `tests/perf_gate.rs` gate at 80 ns/op for same-thread push+pop.

[Unreleased]: ./
[0.1.0]: ./
