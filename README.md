# kevy

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Rust stable](https://img.shields.io/badge/rust-stable-orange.svg)

A pure-Rust, zero-dependency, Redis-compatible key–value store. Use it as
a standalone server, an in-process library, or both — every form speaks
RESP2, so `redis-cli` and every Redis client library work unchanged.

```sh
cargo install kevy
kevy --port 6379 &
redis-cli -p 6379 SET hello world
redis-cli -p 6379 GET hello
```

## What kevy is

kevy ships in three forms, all built from the same engine:

- **Server** — a Redis-wire-compatible daemon. Speaks RESP2, replies are
  reply-checked byte-for-byte against valkey 9.1 for 98 commands.
- **Embedded library** — `kevy-embedded` is the same engine without the
  network. Drop it into a Rust binary and call `Store` directly. Pure
  Rust, zero dependencies, builds for `wasm32`.
- **Clients** — `kevy-client` (blocking) and `kevy-client-async` (one
  feature flag per runtime: tokio / smol / async-std). Both accept a
  URL so the same code targets a TCP server (`kevy://host:port`) or an
  in-process bus (`mem://name`).

## Which one do I want?

| Situation | Use this |
|---|---|
| I have a Redis client library and want a faster, lighter Redis | The server (`kevy`) |
| I have a Rust app and don't want to run a separate process | The embedded library (`kevy-embedded`) |
| I write Rust and want to talk to a kevy or Redis server | `kevy-client` (blocking) |
| I write Rust on `tokio` / `smol` / `async-std` | `kevy-client-async` |
| I want the same code to switch between embed and server with one URL | `kevy-client` + `kevy-embedded` |

## Install

```sh
# Server
cargo install kevy

# Embedded library
cargo add kevy-embedded

# Blocking client
cargo add kevy-client

# Async client (pick one runtime feature)
cargo add kevy-client-async --features tokio
```

Pre-built server binaries are attached to every [GitHub Release](https://github.com/goliajp/kevy/releases)
for Linux x86_64, Linux aarch64, and macOS Apple Silicon. A multi-arch
Docker image is published to both [Docker Hub](https://hub.docker.com/r/goliakk/kevy)
and [GitHub Container Registry](https://github.com/goliajp/kevy/pkgs/container/kevy):

```sh
docker run --rm -p 6379:6379 goliakk/kevy:latest
```

## Quick start

### Server

```sh
kevy --port 6379 &
redis-cli -p 6379 SET foo bar
redis-cli -p 6379 GET foo
```

Configuration precedence is CLI flags → environment variables → TOML
file → built-in defaults. The full annotated schema lives in
[`crates/kevy/kevy.toml.example`](crates/kevy/kevy.toml.example).

### Embedded library

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;
store.set(b"key", b"value")?;
assert_eq!(store.get(b"key")?, Some(b"value".to_vec()));
# Ok::<(), std::io::Error>(())
```

`Store` is `Clone` and every method takes `&self`, so a clone can move
between threads freely. For a file-backed store use
`Config::default().with_persist("/var/lib/myapp")`.

### Blocking client

```rust
use kevy_client::Connection;

let mut conn = Connection::open("tcp://127.0.0.1:6379")?;
conn.set(b"k", b"v")?;
let v = conn.get(b"k")?;
assert_eq!(v.as_deref(), Some(&b"v"[..]));
# Ok::<(), std::io::Error>(())
```

The same URL surface accepts `mem://app` for an in-process backend, so
the same code paths run against an embedded store in tests and a
networked server in production.

### Async client

```rust,no_run
use kevy_client_async::AsyncConnection;

# async fn run() -> std::io::Result<()> {
let mut conn = AsyncConnection::open("tcp://127.0.0.1:6379").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
# Ok(())
# }
```

Pick exactly one of `tokio`, `smol`, or `async-std` as a Cargo feature;
the crate refuses to compile on zero or more than one.

## Performance

A representative slice from the bare-metal benchmark suite (16-core
Linux box, server and client pinned to disjoint cores, TCP loopback,
precision-mode with CI95 < 1%). Full method, every workload, and the
caveats live in [`bench/REPORT.md`](bench/REPORT.md); every figure is
reproducible from a script in [`bench/`](bench/).

| Workload | kevy | valkey 9.1 | Ratio |
|---|---:|---:|---:|
| `SET -c 1` | 94.7 k/s | 62.2 k/s | **1.52×** |
| `GET -c 1` | 97.3 k/s | 65.0 k/s | **1.50×** |
| `SET -c 50 -P 16` | 2.59 M/s | 1.82 M/s | **1.42×** |
| Pub/sub fan-out (50 subs) | 23.1 M/s | 5.1 M/s | **4.52×** |
| Embedded `get` (hit) | 9.0 M/s | — | (no in-process Redis) |
| Embedded `set` (overwrite) | 7.0 M/s | — | (no in-process Redis) |

A complete server is a 768 KB stripped binary that boots into under
5 MB of RSS.

## Compatibility

98 commands are reply-checked byte-for-byte against valkey 9.1,
covering all five Redis data types (String, Hash, List, Set, Sorted
Set) plus Streams, Pub/Sub (channel + pattern), Transactions (`MULTI` /
`EXEC` / `WATCH` / `UNWATCH`), Blocking pops, and the standard
operations and persistence verbs. The full command list is in
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md).

Client libraries verified end-to-end against kevy:

| Language | Library | Version |
|---|---|---|
| Java | [Jedis](https://github.com/redis/jedis) | 5.x |
| .NET | [StackExchange.Redis](https://stackexchange.github.io/StackExchange.Redis/) | 2.x |
| Go | [go-redis](https://github.com/redis/go-redis) | v9 |
| Python | [redis-py](https://github.com/redis/redis-py) | 5.x |
| Python | [Celery](https://docs.celeryq.dev/) | 5.6 |
| Ruby | [Sidekiq](https://sidekiq.org/) | 6.5 |
| Node.js | [ioredis](https://github.com/redis/ioredis) | 5.7 |
| Node.js | [BullMQ](https://github.com/taskforcesh/bullmq) | 5.79 |
| Node.js | [Bee Queue](https://github.com/bee-queue/bee-queue) | 1.7 |
| Node.js | [node-redlock](https://github.com/mike-marcacci/node-redlock) | 5 |

All run unmodified against a default `kevy --port 6379` instance.

## Crates

| Crate | Role |
|---|---|
| [`kevy`](crates/kevy) | The server binary and library entry-point |
| [`kevy-embedded`](crates/kevy-embedded) | In-process KV with the Redis-shaped Rust API |
| [`kevy-client`](crates/kevy-client) | Blocking RESP client; URL facade for server or in-process backend |
| [`kevy-client-async`](crates/kevy-client-async) | Async mirror of `kevy-client` for tokio / smol / async-std |
| [`kevy-cluster-rw`](crates/kevy-cluster-rw) | Primary-write / replica-read client wrapper |
| [`kevy-cli`](crates/kevy-cli) | Operator CLI: backup, restore, smoke tests |
| [`kevy-config`](crates/kevy-config) | TOML config schema with CLI/env/file precedence |
| [`kevy-resp-client`](crates/kevy-resp-client) | Low-level RESP2 client primitive |
| [`kevy-bytes`](crates/kevy-bytes) | Owned byte string with inline-or-heap small-string optimization |
| [`kevy-hash`](crates/kevy-hash) | Fast non-cryptographic hash for single-trust-domain keyspaces |
| [`kevy-map`](crates/kevy-map) | Swiss-table hashmap with SIMD group scan |
| [`kevy-resp`](crates/kevy-resp) | Zero-allocation RESP2 / 3 parser |
| [`kevy-ring`](crates/kevy-ring) | Bounded lock-free SPSC queue |
| [`kevy-madvise`](crates/kevy-madvise) | Linux `MADV_HUGEPAGE` wrapper; no-op elsewhere |
| [`kevy-uring`](crates/kevy-uring) | Pure-Rust io_uring bindings — no liburing linked |
| [`kevy-geo`](crates/kevy-geo) | Geospatial command primitives |
| [`kevy-lua`](crates/kevy-lua) | Lua scripting bridge (backed by the [luna](https://github.com/goliajp/luna) runtime) |

The remaining crates (`kevy-store`, `kevy-rt`, `kevy-persist`,
`kevy-sys`, `kevy-elect`, `kevy-replicate`, `kevy-scope`,
`kevy-lua-host`, `kevy-chaos`, `kevy-bench`, `kevy-pubsub-bench`) are
internal infrastructure for the server and embedded library — they are
published so the workspace builds reproducibly, but end users typically
reach for the surfaces above.

## Topic guides

| Topic | Doc |
|---|---|
| Configuration tuning | [`docs/tuning.md`](docs/tuning.md) |
| Persistence (AOF + RDB) | [`docs/persistence.md`](docs/persistence.md) |
| Pub/Sub | [`docs/pubsub.md`](docs/pubsub.md) |
| Replication | [`docs/replication.md`](docs/replication.md) |
| Cluster mode | [`docs/cluster.md`](docs/cluster.md) |
| Lua scripting | [`docs/lua.md`](docs/lua.md) |
| Unix-domain socket | [`docs/uds.md`](docs/uds.md) |
| Async client | [`docs/async.md`](docs/async.md) |
| WebAssembly build | [`docs/wasm.md`](docs/wasm.md) |
| Accept-shard sizing | [`docs/accept-shards.md`](docs/accept-shards.md) |
| Error reply reference | [`docs/error-replies.md`](docs/error-replies.md) |

## Out of scope

kevy is honest about what it does not do. By charter, these are
permanently out of scope and there is no plan to add them:

- **AUTH and TLS.** kevy assumes a trusted network. Front it with a
  TLS-terminating sidecar (envoy, stunnel) and an authentication proxy
  if you need either.
- **Multi-DC active-active and cross-DC replication.** Single-DC only.
- **Multi-database `SELECT`.** One keyspace per server.
- **ACL.** Single trust domain.
- **Gossip discovery and online resharding.** Cluster topology is
  declarative; resharding is offline.

If you need any of these, Redis Cluster, Valkey, or a hosted KV service
is the right fit.

## Build and test

```sh
cargo build --workspace --release
cargo test  --workspace
```

Stable Rust 1.95, Rust 2024 edition. Builds on Linux (`x86_64`,
`aarch64`) and macOS. `kevy-embedded` and its dependency closure also
build for `wasm32-unknown-unknown` and `wasm32-wasip1`.

## Roadmap and stability

The workspace is on the v2.x line. Persistence format, RESP wire
protocol, public Rust API, CLI flags, env vars, TOML schema, and
eviction semantics are add-only across each major line: a file written
by v2.0 loads on every later v2.x build, and additive features land in
minor releases without breaking earlier code. The full stability
contract is in
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md#v1x-stability-commitment).

## License

Licensed under either of MIT or Apache-2.0, at your option.

© 2026 GOLIA K.K.
