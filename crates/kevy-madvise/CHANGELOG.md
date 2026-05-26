# Changelog

All notable changes to `kevy-madvise` will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-27

### Added

- Initial release.
- `advise_hugepage(ptr, len)` — thin `MADV_HUGEPAGE` wrapper with
  page-rounding and "tiny region = no syscall" short-circuits. Off
  Linux it compile-time no-ops.

### Notes

Carved out of `kevy-sys` per the project's STONE/CEMENT split: the
wrapper is generic enough to be a publishable stone in its own right,
while the rest of `kevy-sys` (sockets + readiness poller) stays cement.
