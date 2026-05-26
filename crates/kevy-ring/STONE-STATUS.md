# kevy-ring — publish status

Snapshot of the verification matrix as of the most recent audit.

## Latest

- **Version**: 0.1.0
- **Audit**: [`AUDIT-2026-05-26.md`](./AUDIT-2026-05-26.md) (T1 PASS, T2 PASS)

## Test runs

| Tool / target              | Toolchain                          | Timestamp     | Result        |
|----------------------------|------------------------------------|---------------|---------------|
| `cargo test`               | stable 1.95                        | 2026-05-26    | PASS          |
| `cargo clippy -Dwarnings`  | stable 1.95                        | 2026-05-26    | 0 findings    |
| `cargo doc`                | stable 1.95                        | 2026-05-26    | 0 warnings    |
| `cargo miri test`          | nightly-2026-05-19                 | 2026-05-26    | 7/7 cross-thread PASS |
| `cargo-llvm-cov` line cov  | stable 1.95                        | 2026-05-26    | 99.11%        |
| `cargo run --example bench_ring --release` | stable 1.95     | 2026-05-26    | within budget |
| `tests/perf_gate.rs`       | stable 1.95 (`--release`)          | 2026-05-26    | PASS (80 ns/op) |

## Loom

Loom-style interleaving tests are **deferred** — `loom` is a crates.io
dependency, which the project's 0-dep charter excludes. Cross-thread
correctness is exercised by `cargo miri test` on the integration tests
under the standard library's `MIRIFLAGS=-Zmiri-strict-provenance`. See
project [memory `feedback-pure-rust-no-c-principle`].

## Fuzz

Not applicable — ring is a typed container, no decode logic to fuzz.

## Known issues

None.
