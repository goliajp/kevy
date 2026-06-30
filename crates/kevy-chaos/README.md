# kevy-chaos

A chaos test harness for kevy. Spawns the server as a child process,
drives concurrent writes over TCP, simulates crashes (abrupt
`SIGKILL`, then restart), and verifies recovery invariants on the
restarted server.

Test-only — not for inclusion in any production deployment graph.

## What this crate provides

- `Harness` — spawn / drive / crash / restart / verify orchestration
  around a kevy child process.
- `WriterPool` — N concurrent writer threads, each capturing its own
  ACK log of `(key, value, ack_seq)` tuples for post-restart
  verification.
- Standard verification flows: zero-loss (`appendfsync = always`)
  and bounded-window (`appendfsync = everysec`).

## What this crate does not provide

- Production server features.
- A test runner. Use `cargo test --workspace --release -- --ignored`
  to opt into the crash tests (they take seconds to minutes per
  test).
- Performance benchmarks. Those live in
  [`kevy-bench`](https://crates.io/crates/kevy-bench).

## Usage

See `crates/kevy/tests/crash_always.rs` and
`crates/kevy/tests/crash_everysec.rs` in the repository for the
canonical usage patterns.

## License

MIT OR Apache-2.0, at your option.
