# kevy-chaos

Chaos test harness for kevy — spawns kevy as a child process, drives concurrent writes via TCP, simulates crashes (abrupt SIGKILL, then restart), verifies invariants on the recovered state.

**Test-only.** Not for production server inclusion.

## What this crate provides

- `Harness` — spawn + drive + crash + restart + verify orchestration around a kevy child process.
- `WriterPool` — N concurrent writer threads, each capturing its own ACK log of `(key, value, ack_seq)` tuples for post-restart verification.
- Standard verification flows: zero-loss (`appendfsync = always`), bounded-window (`appendfsync = everysec`).

## What this crate does NOT provide

- Server features (kevy-chaos is test-only).
- A test runner. Use `cargo test --workspace --release -- --ignored` to opt into the crash tests (they take seconds to minutes per test).
- Performance benchmarks (that's `kevy-bench`).

## Usage

See `crates/kevy/tests/crash_always.rs` + `crates/kevy/tests/crash_everysec.rs` for the canonical usage patterns, and `docs/chaos-tests.md` for the assertion table.
