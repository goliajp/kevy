# Changelog

All notable changes to **kevy-hash** will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-26

### Added

- Initial release of `FxFmixHasher` (Fx absorb + `fmix64` finalizer) and
  `FxFmixBuildHasher` for drop-in use with `std::collections::HashMap`.
- `KevyHash` trait + `kevy_hash::kevy_hash(b: &[u8]) -> u64` zero-state
  helper for callers that don't need `Hasher`'s state machine.
- 3.7–7× faster than `std`'s SipHash on the byte-string + integer key
  shapes kevy uses (see [`BUDGETS.md`](./BUDGETS.md)).

### Verified

- 95%+ line coverage; perf-gate test (`tests/perf_gate.rs`) guards the
  ns budget.
- Avalanche-clustering test on low-entropy keys.

### Caveats

- **Not DoS-resistant.** Designed for single-trust-domain keyspaces.

[Unreleased]: ./
[0.1.0]: ./
