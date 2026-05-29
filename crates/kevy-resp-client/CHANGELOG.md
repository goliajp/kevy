# Changelog

All notable changes to **kevy-resp-client** will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-05-26

### Added

- Initial release. `RespClient::connect(host, port)` + `request(argv)`
  blocking pair. Wraps a `std::net::TcpStream` and a buffered reader,
  drives [`kevy-resp`](https://crates.io/crates/kevy-resp) for parsing.
- Returns `kevy_resp::Reply` directly (no wrapper type) so callers can
  pattern-match on the wire shape.

### Carve-out

This crate was extracted from the in-tree `kevy-cli` binary so the
library part can be a publishable crate (kevy-cli stays a dev tool;
identity = "kevy-cli binary, not a reusable lib").

### Verified

- 7/7 integration tests against a mock RESP server in
  `tests/roundtrip.rs` (PING, GET hit/miss, integer/array/error replies,
  mid-reply server close).

[Unreleased]: ./
[0.1.0]: ./
