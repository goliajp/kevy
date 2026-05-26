# Changelog

All notable changes to **kevy-bytes** will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-26

### Added

- Initial release of `SmallBytes` — 24-byte SSO union with inline payload
  up to 22 bytes and heap path for longer values.
- `from_slice`, `from_vec`, `as_slice`, `len`, `is_empty`, `to_vec`, `Clone`,
  `Drop`, `Eq`, `Hash` (via `kevy-hash`).
- `Send`+`Sync` — single-owner, no shared mutability.
- Const layout assertion (`size_of == 24`, `align_of == align_of::<usize>()`).
- Compile-time guard against big-endian targets.

### Performance

- Inline `from_slice` ≈ 10–15 ns; `clone` ≈ 5–10 ns; `as_slice` ≈ 2–5 ns
  (mac aarch64, loaded host — see [`BUDGETS.md`](./BUDGETS.md)).

### Verified

- 17/17 unit tests under `cargo miri test` on `nightly-2026-05-19`
  (see [`STONE-STATUS.md`](./STONE-STATUS.md)).
- Line coverage 93%+ via `cargo-llvm-cov`.

[Unreleased]: ./
[0.1.0]: ./
