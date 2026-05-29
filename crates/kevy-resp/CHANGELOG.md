# Changelog

All notable changes to **kevy-resp** will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-26

### Added

- Initial release of the RESP2 codec: `parse_command` (multi-bulk + inline)
  + `parse_command_into` (zero-alloc scratch-Argv form), `parse_reply`,
  and `encode_*` family (`encode_simple_string`, `encode_error`,
  `encode_integer`, `encode_bulk`, `encode_null_bulk`, `encode_array_len`).
- `Argv` — two-allocation argv container with `Send`, indexed `&[u8]`
  access and `PartialEq<Vec<Vec<u8>>>` for ergonomic tests.
- `find_crlf` (SWAR'd) helper used by the hot path.
- `#![forbid(unsafe_code)]` — pure-safe Rust.

### Fixed

- **DoS** — `parse_array_reply` previously preallocated
  `Vec::with_capacity(count)` from an attacker-controlled `*999999999\r\n`
  header. Now capped to remaining buffer bytes
  (see commit `a03a064 fix(resp)!: cap parse_array_reply initial Vec capacity (DoS) + audit`).

### Verified

- 96.31% line coverage; perf-gate test gates parse + encode budgets.
- 1h+ libFuzzer run per target (`parse_command`, `parse_reply`).

[Unreleased]: ./
[0.1.0]: ./
