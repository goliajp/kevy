# kevy

A single-machine, **Redis-compatible** key–value server in pure Rust, with
**zero third-party dependencies** — built to push single-box performance toward
the hardware limit. (For distribution, use Redis/Valkey; that is not kevy's
scenario.)

- **Thread-per-core, shared-nothing** runtime (busy-poll, lock-free hot path).
- All five Redis data types — String, Hash, List, Set, Sorted Set — backed by
  modern structures (Swiss tables, a ring-buffer deque, a B-tree), not Redis's
  legacy encodings.
- RESP2 wire protocol; pub/sub; `MULTI`/`EXEC` transactions; cross-shard
  multi-key commands (`MSET`/`MGET`/`SINTER`/…).
- Durable: RDB-style snapshots + an append-only log (AOF).
- The only C touched is the unavoidable OS-boundary libc, hand-bound in one
  crate (`kevy-sys`); everything else is Rust.

```sh
cargo run --release -p kevy --bin kevy -- --port 6379
redis-cli -p 6379 set foo bar
redis-cli -p 6379 get foo
```

Flags: `--bind --port --threads --dir --no-aof` · env: `KEVY_BIND KEVY_PORT
KEVY_THREADS KEVY_DIR KEVY_AOF`.

For same-host clients, point `KEVY_UNIX_SOCKET` at a filesystem path
and the server dual-binds TCP + Unix-domain socket (RESP semantics
identical; ~60–75 % faster than TCP loopback at every workload —
see [`docs/uds.md`](https://github.com/goliajp/kevy/blob/develop/docs/uds.md)):

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
redis-cli -s /tmp/kevy.sock SET foo bar
```

Built from a small stack of reusable crates: `kevy-sys`, `kevy-resp`,
`kevy-store`, `kevy-net`, `kevy-rt`, `kevy-persist`.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE) at your option.
