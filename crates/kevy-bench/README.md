# kevy-bench

A pure-Rust micro-benchmark harness used inside the kevy workspace.
No `criterion`, no `crates.io` dependencies. Reports median and p95
timings plus A/B speedup ratios.

## Audience

Internal harness for the workspace's own benchmark suite. Not intended
as a general-purpose benchmark framework — applications should reach
for [`criterion`](https://crates.io/crates/criterion) when they need
the public-facing tooling.

## License

MIT OR Apache-2.0, at your option.
