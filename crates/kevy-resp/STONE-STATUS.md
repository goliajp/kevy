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
| `parse_command`  | nightly-2026-05-19 + libFuzzer | 3601 s (2026-05-26) | **2 709 796 455 runs** (~750 k execs/s), on-disk corpus 596 entries; saturated at cov 235 / ft 840 | **0 crashes**. (60 s smoke during audit surfaced no issues for this target.) |
| `parse_reply`    | nightly-2026-05-19 + libFuzzer | 3601 s (2026-05-26) | **365 199 468 runs** (~100 k execs/s — ran in parallel with `parse_command`, half-CPU), on-disk corpus 6361 entries; saturated at cov 229 / ft 1195 | **0 crashes** after the DoS fix in commit `a03a064`. Historical crash artifact `crash-4c4ee6…` (the pre-fix DoS) retained at `fuzz/artifacts/parse_reply/` and replayed clean every fuzz run as a regression check. |

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
