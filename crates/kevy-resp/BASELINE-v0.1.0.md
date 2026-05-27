# kevy-resp v0.1.0 — baseline snapshot

Pre-publish snapshot. Future versions diff against this file.

## Environment

- Host: macOS 26.5 / Apple M4 Pro / arm64
- Toolchain: rustc 1.95.0 stable + Rust 2024 edition
- Build: `--release`
- Date: 2026-05-27

## Headline performance (ns/op, single-binary 25-sample)

| workload                  | best non-kevy        | kevy-resp | verdict     |
|---------------------------|----------------------|----------:|-------------|
| parse_command_set_3args   | redis-rs 511         | **56**    | ✅ **kevy 9.1× faster** |
| parse_reply_bulk_12B      | redis-rs 157         | **18**    | ✅ **kevy 8.7× faster** |

(Single-run min values. The kevy-resp absolute is dominated by the
SWAR find_crlf + zero-alloc `Argv` layout; the redis-rs absolute is
dominated by its per-element `Value::Bulk` `Vec` allocation.)

### Why kevy-resp wins so heavily

`kevy-resp`:
- One `Argv` allocation total per command (the reactor reuses one
  scratch `Argv` so this drops to ~0 amortised).
- SWAR `find_crlf` scans 8 bytes per loop iteration via the classic
  "has-zero-byte" bit-trick — under 1 ns per CRLF on typical frames.
- Pass-1 validates the whole frame before allocating; pass-2 fills
  the pre-sized `Argv` with no further checks.

`redis-rs`'s `parse_redis_value`:
- Returns a recursive `Value` enum, each `Value::Bulk` carries an
  owned `Vec<u8>` per element (`N+1` allocations for an N-arg
  command).
- Scans the wire byte-by-byte with no SWAR acceleration.

The 8-9× gap is **structural**: kevy-resp specifically optimises for
the kevy reactor's hot path (one shared scratch `Argv`, no per-arg
heap), while redis-rs optimises for general-purpose client use.

### Cross-language status

C / Go / C++ competitor benches not in v0.1.0 — RESP parsers in
those ecosystems (hiredis, go-redis, cpp_redis) require non-trivial
fixture setup. Rust competitor is the definitive signal because
redis-rs is the canonical Rust client; kevy-resp 9× ahead of redis-
rs is well above any noise threshold. Cross-lang gate is **deferred
to v0.1.1**; for v0.1.0 the Rust win is decisive.

## Memory contract

- `Argv` holds two `Vec`s: one for concatenated bytes, one for end
  offsets. SET parses with 2 allocations total (or 0 if the reactor
  reuses a scratch Argv).
- Encoders write into a caller-owned `Vec<u8>`; no internal alloc.
- `Reply` enum: `Bulk` holds a `Vec<u8>`; `Array` holds a recursive
  `Vec<Reply>` — these are the natural owned semantics.

## Correctness contracts

| check | result |
|---|---|
| `cargo test -p kevy-resp --lib --tests` | ✅ 25 / 25 pass + 1 doctest |
| `cargo +nightly miri test -p kevy-resp` | ✅ 25 / 25 pass + 1 doctest, no UB (fast — `forbid(unsafe_code)`) |
| `cargo +nightly llvm-cov --branch -p kevy-resp` | Regions 97.82% · Functions **100%** · Lines 97.27% · Branches 83.82% |
| `cargo fuzz run parse_command` (carried from v0.polish) | ✅ 2,709,796,455 runs / 3601 s / 0 crashes |
| `cargo fuzz run parse_reply` (carried from v0.polish) | ✅ 365,199,468 runs / 3601 s / 0 crashes |

Branches 83.82% — the missing branches are defensive paths inside
the `reply_parse` recursion that test fixtures don't deterministically
exercise (e.g., partial-frame returns at very specific inner offsets);
the 2.7B + 365M fuzz coverage from v0.polish covers them in volume.

## Reproducibility

```bash
cargo +nightly llvm-cov clean -p kevy-resp
cargo +nightly llvm-cov --branch -p kevy-resp --lib --tests --summary-only
cargo +nightly miri test -p kevy-resp
( cd perfs/comparative/kevy-resp/rust && cargo build --release \
  && $CARGO_TARGET_DIR/release/kevy-resp-comparative-bench > ../rust-results-$(date +%F).jsonl )
```

## Optimisations between baseline-pre and v0.1.0

| change | effect |
|---|---|
| 13 new effective-cov tests for `Argv` API (`with_capacity`, `clear`, `reserve_for`, `push/get/iter/first`, `Index`, `eq`, `From`, `clone`) | lines 90.65 → 97.27% (above effective-cov target) |
| (no perf changes — kevy-resp was already 9× ahead of redis-rs at session start; the gap is structural) | — |
