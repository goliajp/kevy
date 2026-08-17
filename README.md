# kevy

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg?branch=develop)](https://github.com/goliajp/kevy/actions/workflows/ci.yml?query=branch%3Adevelop)
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
  Rust, zero dependencies, feature-tiered from a bare `core` KV up to
  the full index/replication surface — and it reaches both extremes:
  the browser ([`@goliapkg/kevy`](docs/wasm.md) on npm) and 655 KB IoT
  builds ([docs/iot.md](docs/iot.md)).
- **Clients** — `kevy-client` (blocking) and `kevy-client-async` (one
  feature flag per runtime: tokio / smol / async-std). Both accept a
  URL so the same code targets a TCP server (`kevy://host:port`) or an
  in-process bus (`mem://name`).

## kevy 4 — a serving engine, set in stone

3.x declared kevy a **serving engine**: the primary store for
applications that would otherwise run an RDS with a cache in front.
On top of full Redis parity you get declared secondary indexes
(range / unique / CJK full-text / vector ANN, with server-side hybrid
BM25 + KNN fusion) with one-hop hydration, composable views (virtual
and materialized top-K), a CDC feed with exact recovery points (the
built-in outbox), and a migration toolchain with checksummed
verification — all derived-by-construction (maintained in the write
path, never drifting, rebuilt from data). The 3.x mainline adds the
machine faces — a self-describing verb contract (`COMMAND DOCS`,
generated references, an official MCP server in `kevy-mcp`) — and the
availability arc: streaming replication with heartbeat/ACK lag truth,
planned zero-loss handover (`FAILOVER`) plus quorum crash elections,
and an opt-in consistency ladder (`WAIT`, read-your-writes tokens,
bounded staleness, quorum-fenced writes) — see
[docs/availability.md](docs/availability.md).
4.0 sets it in stone: the public Rust API was consolidated once —
one error type (`KevyError`), one builder, borrowed write faces
([docs/UPGRADING.md](docs/UPGRADING.md)) — and is frozen add-only;
the runtime is instance-scoped, so one process can run several
independent kevys; and the same engine now ships to the browser and
to the edge (the two sections below).
4.0 also raises the capacity ceiling: your dataset no longer has to
fit in RAM. **Transparent tiering** gives the store a RAM budget —
cold values spill to a disposable on-disk value log and page back on
access, every command keeps its exact semantics on a cold key, and
the AOF durability contract is untouched, so RAM bounds your keys
and disk bounds your data. The **TABLE layer** compiles relational
declarations — typed columns, secondary indexes, composite `ORDER BY`
paths, even a PG/MySQL schema file via `kevy-sql` — into named
indexes at declare time, so the single-table read path stays a
lookup and index-only queries answer from RAM even when every row is
cold. See [docs/tiering.md](docs/tiering.md) and
[docs/tables.md](docs/tables.md).
Every headline number is gated and re-measured on every train:
hydrated row-list pages p99 < 1ms, write fan-out p99 < 200µs, ANN
recall ≥ 0.9 — see [the design map](docs/designing-on-kevy.md),
[the cookbook](docs/cookbook.md), and
[the validation ledger](bench/VALIDATION-LEDGER.md).

## Which one do I want?

| Situation | Use this |
|---|---|
| I have a Redis client library and want a faster, lighter Redis | The server (`kevy`) |
| I have a Rust app and don't want to run a separate process | The embedded library (`kevy-embedded`) |
| I write Rust and want to talk to a kevy or Redis server | `kevy-client` (blocking) |
| I write Rust on `tokio` / `smol` / `async-std` | `kevy-client-async` |
| I want the same code to switch between embed and server with one URL | `kevy-client` + `kevy-embedded` |

## Install

**Talking to a kevy server needs no kevy package.** It speaks RESP, so
the Redis client your language already has connects unchanged — kevy's
own verbs (`IDX.*`, `VIEW.*`, `TABLE.*`, `FEED.*`) come through that
client's raw-command channel. Six of them run the same ladder against a
live server in CI on every push (**clientgate**):

| Language | Client to install | kevy verbs via |
|---|---|---|
| Node | `npm i redis` / `npm i ioredis` | `sendCommand([...])` / `call(...)` |
| Go | `go get github.com/redis/go-redis/v9` | `client.Do(ctx, ...)` |
| .NET | `dotnet add package StackExchange.Redis` | `db.Execute(...)` |
| Python | `pip install redis` | `execute_command(...)` |
| C | `hiredis` (your package manager) | `redisCommand(...)` |
| Rust | `cargo add kevy-client` | typed, plus `cmd(...)` |

Full examples per language: [docs/clients.md](docs/clients.md).

For the browser, the engine itself ships as an npm package —
`npm install @goliapkg/kevy` ([In the browser](#in-the-browser)).
Native in-process bindings for Node, Python, Go, C#, Java, Swift,
Kotlin, Flutter and React Native live under [`bindings/`](bindings).
Four are on their language registries:

```sh
npm i @goliapkg/kevy-ts                          # Node / TypeScript
go get github.com/goliajp/kevy-go/v5             # Go
```
```xml
<dependency>                                     <!-- Java -->
  <groupId>jp.golia</groupId><artifactId>kevy</artifactId><version>5.3.0</version>
</dependency>
```

The Go module is the remote client; its embedded engine is cgo against
a static library, which a Go module cannot carry, so that half builds
from this tree with `-tags kevy_embedded` (see
[bindings/go](bindings/go)). The rest — Python, C#, Swift, Kotlin,
Flutter, React Native — build from source and are not on PyPI, NuGet,
SwiftPM or pub.dev yet.

The Rust surface is on crates.io:

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
# Ok::<(), kevy_embedded::KevyError>(())
```

`Store` is `Clone` and every method takes `&self`, so a clone can move
between threads freely. For a file-backed store use
`Config::default().with_persist("/var/lib/myapp")`.

### Blocking client

```rust
use kevy_client::Connection;

let mut conn = Connection::connect("tcp://127.0.0.1:6379")?;
conn.set(b"k", b"v")?;
let v = conn.get(b"k")?;
assert_eq!(v.as_deref(), Some(&b"v"[..]));
# Ok::<(), kevy_client::KevyError>(())
```

The same URL surface accepts `mem://app` for an in-process backend, so
the same code paths run against an embedded store in tests and a
networked server in production.

### Async client

```rust,no_run
use kevy_client_async::AsyncConnection;

# async fn run() -> std::io::Result<()> {
let mut conn = AsyncConnection::connect("tcp://127.0.0.1:6379").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
# Ok(())
# }
```

Pick exactly one of `tokio`, `smol`, or `async-std` as a Cargo feature;
the crate refuses to compile on zero or more than one.

## In the browser

kevy runs in the browser as a real store: the npm package
[`@goliapkg/kevy`](https://www.npmjs.com/package/@goliapkg/kevy) ships
the engine compiled to `wasm32-unknown-unknown` behind a hand-written
ES-module loader — no wasm-bindgen, zero dependencies on either side
of the boundary; six files, 496 KB packed (481 KB gzipped over the wire).

```sh
npm install @goliapkg/kevy
```

```js
import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });
db.set("session", "abc123", { ttlMs: 60_000 });
db.subscribe("events", (payload) => { /* fires from any tab */ });
```

- **Durable**: writes stream to OPFS (IndexedDB fallback) as a kevy
  append-only log, byte-compatible with native kevy — a log written
  in the browser replays on a server.
- **Cross-tab pub/sub** over a BroadcastChannel bridge, with the same
  at-most-once contract as the server.
- **Fast where a store needs to be**: 77–86× IndexedDB on point reads,
  166–189× on point writes, and 12.6–17.4× its durable-write rate
  ([`bench/WASM-BENCH.md`](bench/WASM-BENCH.md)).

[docs/wasm.md](docs/wasm.md) has the loader API and the ABI contract;
[kevy.golia.jp/demo](https://kevy.golia.jp/demo/) is the whole thing
live — a browser REPL with persistence and cross-tab pub/sub, no
backend.

## On the edge (IoT)

The same embedded library scales down. `kevy-embedded` is
feature-tiered (`core` / `persist` / `index` / `text` / `vector` /
`replicate` / `listener`; the default is everything), and the `core`
tier compiles to a **655 KB** binary (budget ≤ 700 KB) that opens an
empty store in under 2 MB of RSS — both lines enforced as a ratchet
by [`bench/iotgate.sh`](bench/iotgate.sh).

- Static musl cross-builds for `aarch64` and `armv7` are gated in CI.
- Below Linux entirely: five foundation crates (`kevy-store`,
  `kevy-hash`, `kevy-bytes`, `kevy-map`, `kevy-madvise`) build
  `no_std` + `alloc`, proven on a Cortex-M target
  (`thumbv7em-none-eabihf`).

See [docs/iot.md](docs/iot.md) for the tier table, the budgets, and a
sensor-cache example.

## Performance

A representative slice from the bare-metal benchmark suite (16-core
Linux box, server and client pinned to disjoint cores, TCP loopback).
The KV rows below are `bench/arena.sh`, re-measured 2026-07-19:
median-of-5, throughput read from each server's own command counter
over a timed window. Full method, every workload, and the caveats live
in [`bench/REPORT.md`](bench/REPORT.md); every figure is reproducible
from a script in [`bench/`](bench/).

| Workload | kevy | valkey 9.1 | Ratio |
|---|---:|---:|---:|
| `GET -c 50 -P 16` | 7.42 M/s | 3.06 M/s | **2.43×** |
| `SET -c 50 -P 16` | 6.80 M/s | 1.69 M/s | **4.02×** |
| Pub/sub fan-out (50 subs) | 23.1 M/s | 5.1 M/s | **4.52×** |
| Embedded `get` (hit) | 9.0 M/s | — | (no in-process Redis) |

The same `GET -c 50 -P 16` face, four engines on one box — kevy at 7.42 M/s against each (median-of-5; method and per-engine cycle
accounting in
[`bench/PERF-VERDICT-V4-T9.md`](bench/PERF-VERDICT-V4-T9.md)):

| Engine | kevy's lead |
|---|---:|
| valkey 9.1 | **2.43×** |
| redis 8 | **1.35×** |
| dragonfly | **2.56×** |

These ratios are **lower than the ones published before 2026-07-19**,
and the reason is the ruler, not the engines. The earlier figures read
`redis-benchmark`'s own reported rate, which under `--threads` is
quantized to multiples of its 250 ms sampling timer and therefore
understated — unevenly, so the ratio moved too. Counting server-side
removes the quantization; kevy's own number went UP (6.39 → 7.24 M/s)
and every competitor's went up more. See
[`bench/PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md`](bench/PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md).

The pub/sub and embedded rows are from their own harnesses and were
not part of this re-measurement.

And the serving face vs redis-stack 7.4.7 (RediSearch), same seeded
corpora, recall-aligned ([`bench/PERF-LEDGER.md`](bench/PERF-LEDGER.md)):

| Query class | kevy | RediSearch | Verdict |
|---|---:|---:|---|
| Full-text (BM25 top-10) | 330 qps | 273 qps | **+21% qps**, p95 tie |
| ANN KNN @ recall 1.000 | 0.48 ms | 0.79 ms | **1.64× ahead** |
| GROUP BY top-100 | 1.9 ms | 202.9 ms | **110×** (write-time aggregates) |
| Numeric range + hydrate | 0.19 ms | 0.43 ms | **2.3×** |

A complete server is a 768 KB stripped binary that boots into under
5 MB of RSS.

**Upgrading?** [docs/UPGRADING.md](docs/UPGRADING.md) covers both
hops in one place — 3.x → 4.0 (wire and disk carry over; the Rust
API changed once, with a table and a rule for every rename) and
2.x → 3.x (binary swap + dependency bump). Snapshots and AOF load
as-is across majors in the upgrade direction.

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
| [`kevy-client`](crates/kevy-client) | Blocking RESP client; URL facade for server or in-process backend — **its own version line (2.x), not the workspace's 4.x** |
| [`kevy-client-async`](crates/kevy-client-async) | Async mirror of `kevy-client` for tokio / smol / async-std — **2.x, same line as `kevy-client`** |
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
| [`kevy-wasm`](crates/kevy-wasm) | The browser build: hand-written C ABI + the `@goliapkg/kevy` loader |
| [`kevy-lua`](crates/kevy-lua) | Lua scripting bridge (backed by the [luna](https://github.com/goliajp/luna) runtime) |

The remaining crates (`kevy-store`, `kevy-rt`, `kevy-persist`,
`kevy-sys`, `kevy-elect`, `kevy-replicate`, `kevy-scope`,
`kevy-lua-host`, `kevy-chaos`, `kevy-bench`, `kevy-pubsub-bench`) are
internal infrastructure for the server and embedded library — they are
published so the workspace builds reproducibly, but end users typically
reach for the surfaces above.

**For AI agents & tools**: [`llms.txt`](llms.txt) (machine-first index) ·
[verb reference](docs/verb-reference.md) (all 189 verbs, generated from the
server's own metadata — the same rows `COMMAND DOCS` serves).

## Topic guides

| Topic | Doc |
|---|---|
| RDS workload mapping (SQL → kevy) | [`docs/rds-workloads.md`](docs/rds-workloads.md) |
| Migration playbook & toolchain | [`docs/migration.md`](docs/migration.md) |
| Configuration tuning | [`docs/tuning.md`](docs/tuning.md) |
| Persistence (AOF + RDB) | [`docs/persistence.md`](docs/persistence.md) |
| Pub/Sub | [`docs/pubsub.md`](docs/pubsub.md) |
| Replication | [`docs/replication.md`](docs/replication.md) |
| Cluster mode | [`docs/cluster.md`](docs/cluster.md) |
| Deploying behind a proxy (TLS) | [`docs/deploy-behind-a-proxy.md`](docs/deploy-behind-a-proxy.md) |
| Lua scripting | [`docs/lua.md`](docs/lua.md) |
| Unix-domain socket | [`docs/uds.md`](docs/uds.md) |
| Async client | [`docs/async.md`](docs/async.md) |
| Browser / WASM | [`docs/wasm.md`](docs/wasm.md) |
| Electron apps | [`docs/electron.md`](docs/electron.md) |
| Tauri apps | [`docs/tauri.md`](docs/tauri.md) |
| IoT & feature tiers | [`docs/iot.md`](docs/iot.md) |
| Accept-shard sizing | [`docs/accept-shards.md`](docs/accept-shards.md) |
| Opt-in allocator (`kevy-alloc`) | [`docs/alloc.md`](docs/alloc.md) |
| Error reply reference | [`docs/error-replies.md`](docs/error-replies.md) |

## Out of scope

kevy is honest about what it does not do. By charter, these are
permanently out of scope and there is no plan to add them:

- **AUTH and TLS.** kevy assumes a trusted network. Front it with a
  TLS-terminating sidecar (stunnel, HAProxy, nginx `stream`) and an
  authentication proxy if you need either —
  [`docs/deploy-behind-a-proxy.md`](docs/deploy-behind-a-proxy.md) is
  the recipe, including why an HTTP reverse proxy cannot do this job.
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

Stable Rust 1.97.0, Rust 2024 edition. Builds on Linux (`x86_64`,
`aarch64`) and macOS. `kevy-embedded` and its dependency closure also
build for `wasm32-unknown-unknown` and `wasm32-wasip1`.

## Roadmap and stability

The workspace is on the v4.x line. Persistence format, RESP wire
protocol, public Rust API, CLI flags, env vars, TOML schema, and
eviction semantics are add-only across each major line — and the
on-disk formats carry across majors: a snapshot or AOF written by
v2.0 loads as-is on every 3.x and 4.x build (see
[docs/UPGRADING.md](docs/UPGRADING.md)). Additive features land in
minor releases without breaking earlier code. The full stability
contract is in
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md#v1x-stability-commitment).

## License

Licensed under either of MIT or Apache-2.0, at your option.

© 2026 GOLIA K.K.
