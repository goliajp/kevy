# kevy

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)

A pure-Rust, **zero-dependency**, Redis-compatible key–value server —
built to run as fast as the hardware allows.

kevy speaks the Redis wire protocol (RESP2), so `redis-cli`, `valkey-cli`,
and every Redis client library talk to it **unchanged**. Underneath, the
engine is a modern thread-per-core, shared-nothing design written entirely
in Rust — the only C it touches is the unavoidable OS syscall boundary.

```sh
cargo run -p kevy --bin kevy --release      # loopback, AOF on, port 6004
redis-cli -p 6004 SET hello world
```

## Performance

> Beating valkey 9.1 is the floor, not the goal — kevy targets the
> hardware ceiling.

Measured on a dedicated 16-core Linux box (server cores 0–9, isolated
client cores):

| metric | kevy (io_uring) | valkey 9.1 (io-threads) | ratio |
|--------|----------------:|------------------------:|------:|
| **-c50 SET / sec** | **4.0 M** | 1.5 M | **2.67×** |
| **-c50 GET / sec** | **4.0 M** | 1.7 M | **2.33×** |
| -c1 SET / sec | 88 k | 58 k | 1.52× |
| -c1 GET / sec | 80 k | 65 k | 1.25× |

Against the C reference implementation: **kevy's hand-written io_uring
bindings reach a 148 ns nop round-trip vs liburing 2.9's 152 ns** — at the
Linux kernel floor, with no liburing linked. Each core library crate
benches at noise-floor parity or better than the best open-source
Rust / Go / C / C++ competitor (8 / 8).

Full method + reproduction: [`bench/REPORT.md`](bench/REPORT.md).

## Why kevy

- **Zero crates.io dependencies.** Only `std` + kevy's own crates. Every
  hashmap, hash function, and protocol parser is written in Rust; the sole
  C is the OS boundary (sockets, epoll / io_uring, mmap), bound by hand
  with `unsafe extern "C"` in a single crate.
- **Thread-per-core, shared-nothing.** One reactor + one keyspace shard
  per core, no locks on the hot path; cores coordinate by message passing.
- **Drop-in Redis compatibility.** RESP2 wire protocol, 94-command parity
  with valkey 9.1 — works with redis-rs, go-redis, jedis, ioredis, and the
  rest, no code changes.
- **Durable.** Snapshots + append-only file (AOF) with `appendfsync`
  `always` / `everysec` / `no`, matching Redis semantics.
- **Modern data structures**, not Redis's legacy encodings — all five data
  types reimplemented from scratch.

## Quick start

### As a server

```sh
# Build + run with defaults (loopback only, AOF on, port 6004)
cargo run -p kevy --bin kevy --release

# Or with a TOML config file
cp crates/kevy/kevy.toml.example ./kevy.toml
cargo run -p kevy --bin kevy --release -- --config ./kevy.toml

redis-cli -p 6004 SET foo bar
redis-cli -p 6004 GET foo
```

Precedence is CLI flags > env vars > TOML file > built-in defaults:

```sh
kevy --bind 0.0.0.0 --port 7000 --threads 4 --dir /var/lib/kevy
# env equivalents: KEVY_BIND  KEVY_PORT  KEVY_THREADS  KEVY_DIR  KEVY_AOF
```

See [`crates/kevy/kevy.toml.example`](crates/kevy/kevy.toml.example) for the
fully annotated config schema.

### As an embedded library

```rust
// Cargo.toml: kevy-store = "0.1"
use kevy_store::Store;

let mut s = Store::default();
s.set(b"key".to_vec(), b"value".to_vec(), None, false, false);
assert_eq!(s.get(b"key").unwrap().unwrap(), b"value");
```

## When to use kevy

kevy v1.0 is production-ready for four scenarios:

1. **Local dev** — `cargo run -p kevy` + your favourite Redis client.
2. **docker-compose internal** — `KEVY_BIND=0.0.0.0` inside the network;
   the trust boundary is the docker network itself.
3. **Embedded library** — drop [`kevy-store`](crates/kevy-store) into your
   app: no network, no reactor.
4. **Cache** — fronted by a real database, kevy holds hot data with TTL +
   `maxmemory` + LRU / LFU eviction.

**Out of scope by design:** replication, clustering, AUTH / TLS, and
direct public-internet exposure. For HA / multi-host, use a Kubernetes
StatefulSet or a sidecar-proxy pattern. The full scope rationale and the
94-command parity table live in
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md).

## Crates

kevy ships as small, reusable crates — 8 publishable libraries plus the
server-internal pieces:

| crate | role |
|-------|------|
| [`kevy-bytes`](crates/kevy-bytes) | owned byte string with inline-or-heap small-string optimization |
| [`kevy-hash`](crates/kevy-hash) | fast non-cryptographic hash for single-trust-domain keyspaces |
| [`kevy-map`](crates/kevy-map) | Swiss-table hashmap with SIMD group scan |
| [`kevy-resp`](crates/kevy-resp) | zero-alloc RESP2 / 3 parser |
| [`kevy-ring`](crates/kevy-ring) | bounded lock-free SPSC queue |
| [`kevy-madvise`](crates/kevy-madvise) | Linux `MADV_HUGEPAGE` wrapper, no-op elsewhere |
| [`kevy-uring`](crates/kevy-uring) | pure-Rust io_uring bindings, no liburing |
| [`kevy-resp-client`](crates/kevy-resp-client) | blocking RESP2 client |
| `kevy-config` · `kevy-store` · `kevy-rt` · `kevy-persist` | config, keyspace, runtime, persistence |
| `kevy-sys` | the sole libc boundary (server-internal) |
| `kevy` | the server binary |

## Commands

All five Redis data types — **String, Hash, List, Set, Sorted Set** — plus
**pub/sub**, **transactions** (`MULTI` / `EXEC` / `DISCARD`), persistence
(`SAVE` / `BGSAVE` / `BGREWRITEAOF`), and operations (`INFO` / `CONFIG` /
`CLIENT` / …). Multi-key commands and pub/sub work across the per-core
shards, and `WRONGTYPE` behaves as in Redis.

The full 94-command list with valkey-parity notes is in
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md).

## Build & test

```sh
cargo build --workspace --release
cargo test  --workspace
bash bench/run.sh        # vs-valkey comparison (Linux + Docker)
```

Stable Rust 1.95, Rust 2024 edition. Builds on Linux (`x86_64`, `aarch64`)
and macOS. `kevy-embedded` and its dependency closure also build for
`wasm32-unknown-unknown` / `wasm32-wasip1` — see [`docs/wasm.md`](docs/wasm.md)
for the WebAssembly walkthrough.

## Roadmap & stability

kevy is in the **v1.0.0-rc** feedback period. Everything that v1.x promises
to keep — persistence format, RESP wire protocol, public Rust API, CLI
flags, env vars, TOML schema, eviction semantics — is **add-only across the
v1.x line**: a file written by v1.0 loads on any later v1.x build. The full
stability contract is in
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md#v1x-stability-commitment).

## License

Licensed under either of **MIT** or **Apache-2.0**, at your option.
© 2026 GOLIA K.K.
