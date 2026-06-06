# kevy

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)

A pure-Rust, **zero-dependency**, Redis-compatible key–value store —
usable as a standalone server **or** as an embedded library, built to run
as fast as the hardware allows.

kevy speaks the Redis wire protocol (RESP2), so `redis-cli`, `valkey-cli`,
and every Redis client library talk to it **unchanged**. Underneath, the
engine is a modern thread-per-core, shared-nothing design written entirely
in Rust — the only C it touches is the unavoidable OS syscall boundary.

```sh
cargo run -p kevy --bin kevy --release      # loopback, AOF on, port 6004
redis-cli -p 6004 SET hello world
```

## Why kevy

- **Fast** — 2.3–2.7× valkey 9.1's throughput at high concurrency, 2.7× on
  pub/sub fan-out, and ~18 M ops/s per core when embedded (numbers below).
- **Tiny footprint** — a 768 KB server binary that boots into under 5 MB of
  RAM. Fits a container sidecar, a small VM, or an edge box.
- **Modern architecture** — thread-per-core, shared-nothing, no locks on
  the hot path, io_uring on Linux. No global lock, no GIL-style bottleneck.
- **No supply-chain risk** — zero crates.io dependencies. The whole tree is
  `std` + kevy's own crates; the only C is the OS syscall boundary,
  hand-bound in one crate. There is nothing else to audit.
- **Drop-in compatible** — RESP2 wire protocol, 98-command parity with
  valkey 9.1 (incl. pattern pub/sub and `WATCH`/`UNWATCH` optimistic CAS),
  reply-checked byte-for-byte. Existing clients and tools just work.
- **Embeddable** — `kevy-store` is a plain Rust library: no network, no
  runtime, also builds for `wasm32`. The same engine, in your process.

Honest about scope: kevy is **single-node** — no replication, clustering,
AUTH/TLS, or public-internet exposure (see
[when to use kevy](#when-to-use-kevy)).

## Performance

All figures below were measured on one **bare-metal Intel Core i7-10700K**
(8 cores / 16 threads, 3.8 GHz base / 5.1 GHz boost), 62 GB RAM,
Linux 6.12.90, in-memory. Every benchmark is reproducible with the scripts
in [`bench/`](bench/); full method and caveats in
[`bench/REPORT.md`](bench/REPORT.md).

### Server throughput (over the network)

> Beating valkey 9.1 is the floor, not the goal — kevy targets the
> hardware ceiling.

`redis-benchmark`, each server pinned to cores 0–9 with the client on
isolated cores and run in isolation. Every engine uses its fastest config
(kevy: io_uring at -c50, epoll at -c1; valkey/redis: io-threads):

| workload | kevy | valkey 9.1 | redis 7.4 |
|----------|-----:|-----------:|----------:|
| **-c50 -P16 GET** | **4.4 M/s** | 2.5 M/s | 2.3 M/s |
| **-c50 -P16 SET** | **4.7 M/s** | 1.9 M/s | 2.0 M/s |
| **-c1 GET** | **86 k/s** | 65 k/s | 48 k/s |
| **-c1 SET** | **72 k/s** | 63 k/s | 54 k/s |

Against the C reference for io_uring: kevy's hand-written bindings reach a
148 ns nop round-trip vs liburing 2.9's 152 ns — at the Linux kernel floor,
with no liburing linked. Reproduce with
[`bench/loopback_c50.sh`](bench/loopback_c50.sh) and
[`bench/loopback_c1.sh`](bench/loopback_c1.sh).

### Embedded throughput (in-process, no network)

Drop [`kevy-store`](crates/kevy-store) into your app and call it directly —
no socket, no RESP parsing, no reactor. Single core, `Store` API:

| operation | latency (median) | throughput |
|-----------|-----------------:|-----------:|
| `get` (hit) | 54 ns | ~18.5 M ops/s |
| `get` (miss) | 14 ns | — |
| `set` (overwrite) | 76 ns | ~13 M ops/s |
| `incr` | 86 ns | — |

That's roughly **3× the per-core throughput of the network server** — the
embedded path skips the entire wire layer. Reproduce with
`cargo run -p kevy-store --example bench_keyspace --release`.

### Pub/sub fan-out (server mode)

1 publisher → 50 subscribers, 200 000 messages, 16-byte payload. kevy is
the fastest broker on the TCP / RESP path:

| system | delivered msg/s | vs valkey |
|--------|----------------:|----------:|
| Aeron 1.45 (IPC, shared memory) | 26.5 M | 3.90× |
| **kevy** | **18.2 M** | **2.68×** |
| ZeroMQ 4.3.5 | 9.3 M | 1.37× |
| redis 7.4 | 8.5 M | 1.25× |
| valkey 9.1 | 6.8 M | 1.00× |
| Zenoh 1.9 | 2.7 M | 0.40× |

Aeron's shared-memory IPC is the structural ceiling (no kernel network
stack); among TCP brokers kevy leads — 2× ZeroMQ on the same transport.
Pub/sub is a **server-mode** feature; the embedded library is pure
key–value. Method + the 6-way harness:
[`bench/pubsub-compare/`](bench/pubsub-compare/).

### Binary size & memory

| | |
|---|---|
| Server binary (`release`, stripped) | **768 KB** |
| Server binary (`release-min`, `opt-level="s"`) | **640 KB** |
| Idle RSS (default, 16 threads) | **4.9 MB** |
| Idle RSS (`--threads 1`) | **2.5 MB** |
| Memory per key (at 8.6 M keys) | ~190 B (key + value + table overhead) |

`SmallBytes` inlines payloads ≤ 22 B with zero heap allocation. A complete
kevy server is a sub-megabyte binary that boots into under 5 MB of RAM.

## Quick start

### Install

Pre-built `kevy` server binaries are attached to every
[GitHub Release](https://github.com/goliajp/kevy/releases). Supported targets:

| platform | archive |
|----------|---------|
| Linux x86_64 | `kevy-<TAG>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 | `kevy-<TAG>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `kevy-<TAG>-aarch64-apple-darwin.tar.gz` |

> Windows: kevy's OS layer is POSIX sockets + epoll/kqueue + io_uring, so
> there is no native Windows build. Use the Docker image below — Docker
> Desktop on Windows runs Linux containers transparently.

One-liner (Linux / macOS — pick your target):

```sh
TAG=v1.2.0
TARGET=x86_64-unknown-linux-gnu      # or aarch64-unknown-linux-gnu, aarch64-apple-darwin
curl -L "https://github.com/goliajp/kevy/releases/download/$TAG/kevy-$TAG-$TARGET.tar.gz" | tar -xz
sudo install "kevy-$TAG-$TARGET/kevy" /usr/local/bin/kevy
kevy --port 6004
```

Each archive ships the `kevy` binary plus `kevy.toml.example`, `README.md`,
and both license files. A matching `.sha256` is published alongside each
asset. Or build from source as below.

### Run with Docker

The official image is published on every release to **both** Docker Hub
([`goliakk/kevy`](https://hub.docker.com/r/goliakk/kevy)) and GitHub
Container Registry
([`ghcr.io/goliajp/kevy`](https://github.com/goliajp/kevy/pkgs/container/kevy)),
multi-arch (`linux/amd64` + `linux/arm64`). Tags on both registries:
`:<semver>` (e.g. `:1.0.0-rc6`), `:rc` (rolling latest RC), and `:latest`
(stable releases only — never RC).

```sh
# One-shot
docker run --rm -p 6379:6379 goliakk/kevy:rc

# Persistent (snapshot + AOF survive restarts via a named volume)
docker run -d --name kevy -p 6379:6379 -v kevy-data:/data goliakk/kevy:rc
redis-cli -p 6379 SET foo bar
```

Image defaults: `KEVY_BIND=0.0.0.0`, `KEVY_PORT=6379`, `KEVY_DIR=/data`,
`KEVY_AOF=1`. Override any with `-e` or by passing flags after the image:
`docker run ... goliakk/kevy:rc --threads 4 --port 7000`.

Linux hosts running on kernel 5.13+ can opt into the io_uring reactor —
docker's default seccomp profile blocks `io_uring_setup`, so allow it:

```sh
docker run --rm -p 6379:6379 -e KEVY_IO_URING=1 \
  --security-opt seccomp=unconfined goliakk/kevy:rc
```

Prefer the GitHub registry? Swap any `goliakk/kevy` above for
`ghcr.io/goliajp/kevy` — identical image, same tags.

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

kevy is production-ready for four scenarios:

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
98-command parity table live in
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

## Embedded ↔ server with one URL

[`kevy-client`](crates/kevy-client) v1.6.0+ + [`kevy-embedded`](crates/kevy-embedded)
v1.1.0+ let the same code switch between an in-process backend and a TCP
kevy server with a single URL string — including pub/sub (channels +
patterns), `WATCH`-driven transactions, and typed `Transaction::exec_typed`
reply cursors:

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let url = std::env::var("KEVY_URL").unwrap_or_else(|_| "mem://app".into());
let mut sub  = Subscriber::open(&url, &[b"events"])?;  // consumer
let mut conn = Connection::open(&url)?;                // producer
let _ack = sub.recv()?;                                 // drain SUBSCRIBE ack
conn.publish(b"events", b"hello")?;
match sub.recv()? {
    PubsubEvent::Message { channel, payload } => { /* same code in dev + prod */ }
    _ => {}
}
# Ok::<(), std::io::Error>(())
```

| URL | Backend |
|---|---|
| `mem://` | anonymous in-process, per-open fresh — no shared bus |
| `mem://<name>` | shared in-process bus keyed by `<name>` |
| `file:///abs/path` | shared in-process with snapshot + AOF persistence |
| `kevy://host:port` · `redis://…` · `tcp://…` | TCP RESP server |

Full walkthrough + caveats: [`docs/pubsub.md`](docs/pubsub.md).

## Commands

All five Redis data types — **String, Hash, List, Set, Sorted Set** — plus
**Streams** (`XADD` / `XREAD` / `XRANGE` / consumer groups), **blocking
pops** (`BLPOP` / `BRPOP` / `XREAD BLOCK` / `XREADGROUP BLOCK` — single- and
multi-key, **across shards**), **pub/sub** (`SUBSCRIBE` / `PSUBSCRIBE` —
pattern glob), **transactions** (`MULTI` / `EXEC` / `DISCARD` / `WATCH` /
`UNWATCH` — optimistic CAS), persistence (`SAVE` / `BGSAVE` /
`BGREWRITEAOF`), and operations (`INFO` / `CONFIG` (real hot-modification) /
`CLIENT` / …). Multi-key commands, pub/sub, WATCH, and blocking pops all
work across the per-core shards, and `WRONGTYPE` behaves as in Redis.

The full 98-command list with valkey-parity notes is in
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

kevy is in the **v1.x line** (current workspace v1.2.x, with v1.3.0
in flight). Everything that v1.x promises to keep — persistence
format, RESP wire protocol, public Rust API, CLI flags, env vars,
TOML schema, eviction semantics — is **add-only across the v1.x line**:
a file written by v1.0 loads on any later v1.x build, and additive
features (WATCH, pattern pub/sub, real CONFIG SET, typed transaction
cursors) land in minor releases without breaking earlier code. The
full stability contract is in
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md#v1x-stability-commitment).

## License

Licensed under either of **MIT** or **Apache-2.0**, at your option.
© 2026 GOLIA K.K.
