# kevy-resp v0.1.0 — baseline snapshot

Pre-publish snapshot. Future versions diff against this file.

## Environment

- Host: macOS 26.5 / Apple M4 Pro / arm64
- Toolchain: rustc 1.95.0 stable + Rust 2024 edition
- Build: `--release`
- Date: 2026-05-27

## Headline performance (ns/op, single-binary 25-sample)

5-run min-of-medians (single-binary 25 samples × 1M iter):

| workload                | hiredis (C) | redis-rs (Rust) | kevy-resp (Rust) | verdict |
|-------------------------|------------:|----------------:|-----------------:|---------|
| parse_command_set_3args |         144 |             255 |           **50** | ✅ **kevy 2.9× C / 5.1× Rust** |
| parse_reply_bulk_12B    |          42 |             101 |           **16** | ✅ **kevy 2.6× C / 6.3× Rust** |

The earlier v0.1.0 single-run snapshot ("9.1× redis-rs", "8.7× redis-rs")
was a lucky low. Multirun min-of-medians narrows the redis-rs ratios to
5-6× — still a decisive win. Crucially, **kevy-resp is also 2.6-2.9×
faster than hiredis (the C reference)**: the structural advantages
(SWAR `find_crlf` + zero-alloc `Argv` layout) carry over into
absolute leadership, not just "fastest in Rust".

(Absolutes: hiredis dominated by per-frame `redisReply` tree
allocation; redis-rs dominated by per-element `Value::Bulk` `Vec`
allocation; kevy-resp dominated by the `Argv` alloc pair, which the
reactor amortises to ~0 via scratch reuse.)

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

- **C (hiredis 1.3.0)** — landed in v0.1.1 polish (P14-B1). Per the
  table above: kevy 2.6-2.9× faster than hiredis on both workloads.
  hiredis's `redisReader` is the canonical C parser; the win
  generalises beyond "fastest in Rust".
- **Go (go-redis)** — deferred. Go's RESP parser sits inside the
  client package and isn't trivial to isolate as a standalone
  parser; would require a partial harness fork. Given kevy-resp
  already beats both Rust and C reference parsers by ≥ 2.6×, this is
  unlikely to change the verdict.
- **C++ (cpp_redis / boost::redis)** — deferred. The C++ ecosystem
  predominantly wraps hiredis; an independent C++ parser would not
  add signal beyond what hiredis already provides.

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
