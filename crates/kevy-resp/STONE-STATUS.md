# kevy-resp — publish status

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
| `cargo miri test`          | nightly-2026-05-19                 | 2026-05-26    | PASS (forbid-unsafe-code) |
| `cargo-llvm-cov` line cov  | stable 1.95                        | 2026-05-26    | 96.31%        |
| `cargo run --example bench_resp --release` | stable 1.95     | 2026-05-26    | within budget |
| `tests/perf_gate.rs`       | stable 1.95 (`--release`)          | 2026-05-26    | PASS          |

## Fuzz

| Target           | Toolchain (cargo-fuzz) | Wall-clock | Execs / corpus | Findings |
|------------------|------------------------|------------|----------------|----------|
| `parse_command`  | nightly-2026-05-19 + libFuzzer | ≥ 3600 s  | recorded in `fuzz/corpus/parse_command/` | **0 crashes** (initial 60s catch surfaced the DoS in `parse_array_reply`; fixed in commit `a03a064` and re-fuzzed clean). |
| `parse_reply`    | nightly-2026-05-19 + libFuzzer | ≥ 3600 s  | recorded in `fuzz/corpus/parse_reply/`   | **0 crashes** after the DoS fix. |

Run command (any time):

```bash
cd crates/kevy-resp && cargo +nightly-2026-05-19 fuzz run parse_command -- -max_total_time=3600 -timeout=10
cd crates/kevy-resp && cargo +nightly-2026-05-19 fuzz run parse_reply   -- -max_total_time=3600 -timeout=10
```

## Known issues

None.

## Snapshot policy

When `cargo miri test` or coverage drops, the line is re-dated rather than
historic-appended. Long-form history lives in commit messages + `CHANGELOG.md`.
