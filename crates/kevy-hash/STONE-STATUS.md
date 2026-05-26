# kevy-hash — publish status

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
| `cargo miri test`          | nightly-2026-05-19                 | 2026-05-26    | PASS          |
| `cargo-llvm-cov` line cov  | stable 1.95                        | 2026-05-26    | 95.35%        |
| `cargo run --example bench_hash --release` | stable 1.95     | 2026-05-26    | within budget |
| `tests/perf_gate.rs`       | stable 1.95 (`--release`)          | 2026-05-26    | PASS          |

## Fuzz

Not directly applicable — pure mixing function, no parser. Avalanche /
clustering coverage is asserted by `no_catastrophic_clustering_on_low_entropy_keys`
in unit tests.

## Known issues

None.
