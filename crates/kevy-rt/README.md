# kevy-rt

A shared-nothing, **thread-per-core** runtime in pure Rust. Part of
[kevy](https://crates.io/crates/kevy).

Each core runs its own [kevy-net](https://crates.io/crates/kevy-net) reactor and
owns one shard of the keyspace (`hash(key) % nshards`). There is **no shared
mutable state and no lock on the hot path** — cores coordinate only by message
passing, woken via a self-pipe. Connections spread across cores by
`SO_REUSEPORT`; a command whose key lives on another core is forwarded there,
executed, and the reply routed back, with **per-connection reply ordering**
preserved (RESP is pipelined).

- Adaptive **busy-poll**: a spinning core sees cross-core messages with no
  wakeup syscall; it parks (with a backstop timeout) only when idle.
- Cross-shard fan-out + gather for multi-key commands; cross-core pub/sub
  delivery; per-connection transaction (MULTI/EXEC) state.
- Command set injected via the `Commands` trait — the runtime is independent of
  any particular protocol. `Store` is re-exported for convenience.
- `#![forbid(unsafe_code)]`.

Implement `Commands`, then `Runtime::new(ip, port, nshards, cmds).run(stop)`.
See the crate docs for a complete example.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
