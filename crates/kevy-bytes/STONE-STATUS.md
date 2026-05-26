# kevy-bytes — publish status

Snapshot of the verification matrix as of the most recent audit. Update on
every release.

## Latest

- **Version**: 0.1.0
- **Audit**: [`AUDIT-2026-05-26.md`](./AUDIT-2026-05-26.md) (T1 PASS, T2 PASS)

## Test runs

| Tool / target              | Toolchain                          | Timestamp     | Result        |
|----------------------------|------------------------------------|---------------|---------------|
| `cargo test`               | stable 1.95                        | 2026-05-26    | 17/17 PASS    |
| `cargo clippy -Dwarnings`  | stable 1.95                        | 2026-05-26    | 0 findings    |
| `cargo doc`                | stable 1.95                        | 2026-05-26    | 0 warnings    |
| `cargo miri test`          | nightly-2026-05-19                 | 2026-05-26    | 17/17 PASS    |
| `cargo-llvm-cov` line cov  | stable 1.95                        | 2026-05-26    | 93.24%        |
| `cargo run --example bench_sso --release` | stable 1.95     | 2026-05-26    | within budget |
| `tests/perf_gate.rs`       | stable 1.95                        | 2026-05-26    | PASS          |

## Fuzz

Not applicable — `SmallBytes` is not a parser; `from_slice`/`from_vec` are
length-bounded copies with no decode logic. Fuzzing would only re-test
`memcpy` itself.

## Known issues

None.

## Snapshot policy

When `cargo miri test` or coverage drops, the line is re-dated rather than
historic-appended; the file represents "the version we'd publish *right now*",
not a log. Long-form history lives in commit messages.
