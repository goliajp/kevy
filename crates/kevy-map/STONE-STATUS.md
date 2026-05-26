# kevy-map — publish status

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
| `cargo miri test`          | nightly-2026-05-19                 | 2026-05-26    | PASS (inline asm `cfg(not(miri))`-gated) |
| `cargo-llvm-cov` line cov  | stable 1.95                        | 2026-05-26    | 98.79%        |
| `cargo run --example bench_vs_std --release` | stable 1.95   | 2026-05-26    | within budget |
| `tests/perf_gate.rs`       | stable 1.95 (`--release`)          | 2026-05-26    | PASS          |

## Fuzz

Not directly applicable in the standalone crate (Map ops are infallible
on valid `Eq + Hash` keys). The wire-level path that feeds into KevyMap
(via `kevy-store`) is fuzzed end-to-end through kevy-resp.

## Known issues

None.
