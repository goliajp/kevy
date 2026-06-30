# kevy-pubsub-bench

A pub/sub fan-out throughput benchmarker. Raw RESP over TCP; works
against kevy, valkey, or redis. Pure Rust, zero `crates.io`
dependencies.

Used to measure publisher → N-subscriber throughput across servers at
varying subscriber counts and message sizes. The harness configuration
and results live alongside the rest of the benchmarks in
[`bench/`](https://github.com/goliajp/kevy/tree/develop/bench).

## Audience

Internal harness. Not intended as a standalone tool.

## License

MIT OR Apache-2.0, at your option.
