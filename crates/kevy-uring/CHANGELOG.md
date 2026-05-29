# Changelog

All notable changes to `kevy-uring` will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-27

### Added

- Initial release. Carved out of `kevy-sys` (the OS-boundary layer for
  kevy) into its own publishable crate — the engine is generic Linux
  infrastructure, not kevy-specific.
- `IoUring::new(entries)` — allocate a SQ + CQ + SQEs ring via
  `io_uring_setup` and `mmap` the three kernel-shared regions.
- SQE preparation: `prep_nop`, `prep_accept`, `prep_read`, `prep_write`,
  `prep_recv_multishot`. Submission and reaping via `submit_and_wait` +
  `for_each_completion`.
- `ProvidedBufRing` — `register_buf_ring(entries, buf_size, bgid)` +
  buffer recycling, enabling kernel-picked buffer placement on multishot
  `recv` (kernel 5.19+).
- Linux-only crate (`#![cfg(target_os = "linux")]`). Compiles to an
  empty module on every other target so consumers can `cfg`-gate at the
  call site without `target_os` clutter in their `Cargo.toml`.
