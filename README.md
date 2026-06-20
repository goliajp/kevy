# kevy

**English** · [简体中文](README.zh-CN.md) · [日本語](README.ja.md)

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Rust stable](https://img.shields.io/badge/rust-stable-orange.svg)

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

- **Fast** — 2.4–2.5× valkey 9.1's throughput at high concurrency, 2.7× on
  pub/sub fan-out, and **~9 M GET / 7 M SET** per core when embedded
  (numbers below).
- **Tiny footprint** — a 768 KB server binary that boots into under 5 MB of
  RAM. Fits a container sidecar, a small VM, or an edge box.
- **Modern architecture** — thread-per-core, shared-nothing, no locks on
  the hot path, io_uring on Linux. No global lock, no GIL-style bottleneck.
- **No supply-chain risk** — zero crates.io dependencies in the default
  server / blocking-client / embedded stack. The whole tree is `std` +
  kevy's own crates; the only C is the OS syscall boundary, hand-bound in
  one crate. The async client (`kevy-client-async`) is the sole carved
  exemption — opt-in, lib consumers only, fully documented.
- **Drop-in compatible** — RESP2 wire protocol, 98-command parity with
  valkey 9.1 (incl. pattern pub/sub and `WATCH`/`UNWATCH` optimistic CAS),
  reply-checked byte-for-byte. Existing clients and tools just work.
- **Replicated** (v1.22) — server primary + N read replicas, quorum
  failover, and **embed nodes can join the cluster** as read-replicas or
  per-prefix writers. Same wire protocol throughout, single declarative
  topology.
- **Embeddable** — `kevy-store` is a plain Rust library: no network, no
  runtime, also builds for `wasm32`. The same engine, in your process.
- **Async-capable** — `kevy-client-async` (v1.22) wraps the blocking
  surface 1:1 for `tokio` / `smol` / `async-std` plus a pipeline-first
  builder that collapses N commands into one TCP round-trip.
- **Resource-adaptive** — runs full-speed when memory is unbounded, degrades
  cleanly when it isn't, and refuses loudly at the edge instead of corrupting
  silently ([details](#resource-adaptive-by-design)).

Honest about scope: kevy is **single-DC**, with no AUTH/TLS and no
public-internet exposure design (see [when to use kevy](#when-to-use-kevy)).
Replication is single-DC primary-replica with quorum failover; cross-DC
active-active, gossip, online resharding, and Raft are explicitly
out-of-scope.

## Performance

All figures below were measured on one **bare-metal 16-core Linux box**
(lx64), in-memory, with server / client / loadgen pinned to disjoint
cores. Every benchmark is reproducible with the scripts in
[`bench/`](bench/); full method, caveats, and the v0.2 → v1.22
chronological narrative live in [`bench/REPORT.md`](bench/REPORT.md).

### Server throughput (over the network)

> Beating valkey 9.1 is the floor, not the goal — kevy targets the
> hardware ceiling.

`redis-benchmark`, each server pinned to cores 0–9 with the client on
isolated cores and **run in isolation** (start → 2 warm runs → stop) so
kevy's busy-poll does not starve a co-located competitor. Every engine
uses its fastest config (valkey/redis with `--io-threads 10`):

| workload | kevy 1.23 | valkey 9.1 (io-threads) | redis 7.4 (io-threads) |
|----------|----------:|------------------------:|-----------------------:|
| **-c50 -P16 GET** | **6.0 M/s** | 2.4 M/s | 1.5 M/s |
| **-c50 -P16 SET** | **4.0 M/s** | 1.7 M/s | 1.2 M/s |
| **-c1 GET** | **84 k/s** | 69 k/s | 63 k/s |
| **-c1 SET** | **84 k/s** | 64 k/s | 62 k/s |

→ kevy is **2.5× best-other on GET, 2.4× on SET** at high concurrency,
and the -c1 lead grew to **1.22-1.31×** vs the strongest competitor
(v1.22 was 1.13-1.26×) thanks to the v1.23 perf sprint:
profile-driven kernel + reactor wins documented in
[`bench/PERF-ATTACK-LOG-2026-06-20.md`](bench/PERF-ATTACK-LOG-2026-06-20.md)
and the [`CHANGELOG`](CHANGELOG.md).
io_uring vs epoll reactor pick is per-workload (io_uring leads at low
concurrency, epoll catches up at -c50 -P16 where pipelining amortises
the syscall savings). The -c50-P16 SET/GET above are
`redis-benchmark` client-side caps (the server has more headroom).
Reproduce with [`bench/loopback_c50.sh`](bench/loopback_c50.sh) and
[`bench/loopback_c1.sh`](bench/loopback_c1.sh).

Against the C reference for io_uring: kevy's hand-written bindings reach
a 148 ns nop round-trip vs liburing 2.9's 152 ns — at the Linux kernel
floor, with no liburing linked.

### Cluster routing (key-aware client)

A single-port client that lands on the wrong shard pays an internal
cross-shard forwarding hop. The cluster-aware [`ClusterClient`](#cluster-mode-single-node-key-aware-routing)
routes each key straight to its owner shard and removes that hop. Clean
lx64 16-core box, server/client on disjoint cores, GET at concurrency 64:

| client path | throughput | p99 latency |
|-------------|-----------:|------------:|
| single-shard proxy (cross-shard hop) | 333 k/s | 3858 µs |
| **`ClusterClient` (zero hop)** | **533 k/s** | **260 µs** |

**1.6× throughput, ~15× lower tail latency** — purely from removing the
forwarding hop, with no measurable overhead vs a hand-rolled raw router.
Full method in [`docs/cluster.md`](docs/cluster.md).

### Cluster mode (replication + failover + embed-join)

v1.22 closes the v3-cluster track. A kevy node can run as a **primary**
that streams every applied mutation to N replicas, or as a **replica**
that mirrors a primary; **embed nodes can join the cluster** as
read-replicas or as per-prefix writers; and `kevy-elect` performs
quorum-based **automatic failover** when the primary goes DOWN.
Companion client `kevy-cluster-rw` splits writes to the primary and
round-robins reads across replicas.

```toml
# primary
[replication]
role = "primary"
listen_port_base = 16004

# replica
[replication]
role = "replica"
upstream = "primary.example:16004"
```

```sh
# Re-target / promote at runtime via Redis-compat commands.
redis-cli -p 6004 REPLICAOF primary.example 16004
redis-cli -p 6004 REPLICAOF NO ONE
redis-cli -p 6004 ROLE
```

Coverage by phase (all merged in v1.22):
- **Phase 1** (v1.18): per-shard wire backlog + listener, snapshot ship
  for fall-behind replicas, dynamic REPLICAOF / `REPLICAOF NO ONE`
  retarget + demote, `ROLE` / `INFO replication` live state, the
  `kevy-cluster-rw` write-to-primary-read-from-replicas client.
- **Phase 1.5** (v1.19): `kevy-elect` quorum-based automatic primary
  failover (DOWN detection by heartbeat, OFFER/ACCEPT/ANNOUNCE,
  highest-offset wins).
- **Phase 2** (v1.22): **embed nodes can join a cluster as
  read-replicas** — an application embedding `kevy-embedded`
  subscribes to a server primary's replication stream and mirrors
  the keyspace in-process. Reads pay zero network round-trip; local
  writes return `READONLY`.
- **Phase 3** (v1.22): **scoped multi-writer** — `[cluster] scopes =
  "app:billing:=embed-a,app:catalog:=embed-b"` declares per-prefix
  writer ownership; any node receiving a write to the wrong prefix
  answers `-MISDIRECTED writer is <host:port>`. Operator-issued
  `MOVE-SCOPE` migrates a prefix between writers under a
  quiesce-window protocol.

Anti-scope (permanently out): multi-master with overlap, cross-DC
active-active / CRDTs, Raft, gossip discovery, online resharding,
AUTH/TLS.

Full server + client recipes in [`docs/replication.md`](docs/replication.md)
and [`docs/cluster.md`](docs/cluster.md).

### Embedded throughput (in-process, no network)

Drop [`kevy-embedded`](crates/kevy-embedded) into your app and call the
`Store` directly — no socket, no RESP parsing, no reactor. Lx64
in-process bench (1 M ops, 12-byte key, 16-byte value):

| operation | latency | throughput |
|-----------|--------:|-----------:|
| `get` (hit) | 111 ns | **9.0 M ops/s** |
| `get` (miss) | 24 ns | **42.2 M ops/s** |
| `set` (overwrite) | 143 ns | **7.0 M ops/s** |
| `incr` | 169 ns | 5.9 M ops/s |
| `del` | 183 ns | 5.5 M ops/s |

Reproduce with `cargo run -p kevy-embedded --example
embed_throughput --release`.

#### Same Rust caller, 4 backends

The fair comparison: **the same Rust program** with only the backend
swapped — that's what an actual application sees. Single connection,
sequential, N=200k SET + N GET; server columns all go through the
**same** `kevy_client::Connection` RESP path, only the URL differs:

| backend (same Rust caller) | SET ops/s | GET ops/s |
|----------------------------|----------:|----------:|
| **kevy 1.22 embed** | **10.10 M** | **13.76 M** |
| **kevy 1.22 server (io_uring)** | **63.5 k** | **64.4 k** |
| valkey 9.1 server @ localhost | 54.6 k | 53.8 k |
| redis 7.4 server @ localhost | 62.3 k | 61.7 k |

Embed is **~160× faster on SET, ~214× on GET** than calling the same
kevy over TCP-loopback. That's the quantified cost of "no socket, no
protocol, no reactor" for an app that can embed. **Not** a
kevy-vs-valkey/redis throughput claim driven by embed — valkey and
redis have no in-process mode, so the structural gap is unavoidable.
Reproduce with `cargo run -p kevy-embedded --example
embed_vs_server --release --kevy-port 7011 --valkey-port 7012
--redis-port 7013 -N 200000`.

### Pub/sub fan-out (server mode)

1 publisher → 50 subscribers, 200 000 messages, 16-byte payload,
warm-run. kevy is the fastest broker on the TCP / RESP path:

| system | delivered msg/s | vs valkey |
|--------|----------------:|----------:|
| Aeron 1.45 (IPC, shared memory) | 84 M | 12.4× |
| **kevy 1.22** | **18.5 M** | **2.72×** |
| ZeroMQ 4.3.5 | 9.4 M | 1.38× |
| redis 7.4 | 8.9 M | 1.31× |
| valkey 9.1 | 6.8 M | 1.00× |
| Zenoh 1.9 | 2.9 M | 0.43× |

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

Tuning the box itself (CPU pinning, AOF, io_uring, Spectre mitigations) is
documented in [`docs/tuning.md`](docs/tuning.md).

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

On Linux, kevy **auto-selects io_uring** when the host can build the ring
(kernel ≥ 5.19, and `io_uring_setup` not blocked by seccomp) and otherwise
falls back to the epoll reactor — startup never fails on either. Docker's
default seccomp profile blocks `io_uring_setup`, so the default image runs
on epoll; allow io_uring for the faster reactor:

```sh
docker run --rm -p 6379:6379 \
  --security-opt seccomp=unconfined goliakk/kevy:rc
```

Override the auto-pick with `KEVY_IO_URING=0` (force epoll) or
`KEVY_IO_URING=1` (force io_uring — fail loudly if unavailable, for
benchmarks). macOS/BSD always use kqueue.

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

### Cluster mode (single node, key-aware routing)

`--cluster` (or `KEVY_CLUSTER=1` / `[cluster] enabled = true`) exposes each
shard as a virtual cluster node: shard `i` gets a deterministic extra port at
`port + 1 + i`, `CLUSTER SLOTS / SHARDS / NODES` report the real topology
(CRC16 `{hashtag}` slots, one contiguous range per shard), and a wrong-shard
key on a cluster port answers `-MOVED` instead of being forwarded. Stock
cluster-aware clients (`redis-cli -c`, `redis-benchmark --cluster`, client
libraries) then talk straight to the owning shard — no cross-shard forwarding
tax. The main port keeps full proxy-style behaviour for everything else.

```sh
kevy --threads 8 --cluster          # main port 6004, shard ports 6005-6012
redis-cli -c -p 6005 SET foo bar    # follows MOVED automatically
```

For Rust callers, [`kevy-client`](crates/kevy-client) 1.9.0 ships a typed
`ClusterClient` — discover the topology once, then route every key to its
owner shard with no `-MOVED` and no forwarding hop (the **1.6× throughput /
15× tail-latency** win above):

```rust
// Cargo.toml: kevy-client = "1.11"
use kevy_client::ClusterClient;

let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;  // any shard port as seed
cc.set(b"user:42", b"alice")?;                            // routed by CRC16 slot
let v = cc.get(b"user:42")?;
let removed = cc.del(&[b"a", b"b", b"c"])?;               // multi-key may span shards
# Ok::<(), std::io::Error>(())
```

It wraps string / hash / list / set / sorted-set / del / exists / dbsize /
flushall / ping / publish; full guide, command table, and same-slot rules in
[`docs/cluster.md`](docs/cluster.md). Use it when one client drives enough
load that the hop shows up; the plain single-port `Connection` stays correct
and simpler for ordinary use.

Superset notes vs Redis Cluster (single machine cluster mode — no
gossip / MIGRATE-ASK / online resharding): cross-slot multi-key
commands (`MGET`, `SUNION`, transactions, blocking fan-outs) execute
instead of failing with `-CROSSSLOT`, and keyspace-wide views
(`KEYS`, `SCAN`, `DBSIZE`) stay whole-keyspace on every port.
Switching an existing data dir in or out of cluster mode re-homes
keys once at startup (sources are backed up as `*.premigration.<ts>`).

For multi-node clusters with primary + replicas + automatic
failover, see the **Cluster mode (replication + failover)** section
below — v1.22 ships server-as-replica, embed-as-replica, scoped
multi-writer, and quorum-based promotion.

### As an async-runtime client

Apps already running on `tokio`, `smol`, or `async-std` can use the
async mirror of the blocking client:

```rust
// Cargo.toml: kevy-client-async = { version = "1", features = ["tokio"] }
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;

// Pipeline N commands into one TCP round-trip:
let replies = conn.pipeline()
    .set(b"a", b"1").get(b"a").incr(b"hits")
    .run(&mut conn).await?;
# Ok::<(), std::io::Error>(())
```

Exactly one runtime feature must be selected (`tokio`, `smol`, or
`async-std`); the crate compile-errors on zero or more than one. The
blocking [`kevy-client`](crates/kevy-client) stays the default and
remains 0-dep — async is opt-in. Full guide + runtime comparison +
when-to-pipeline:
[`docs/async.md`](docs/async.md).

### As an embedded library

```rust
// Cargo.toml: kevy-embedded = "1.4"
use kevy_embedded::{Config, Store};

let s = Store::open(Config::default().without_aof())?;
s.set(b"key", b"value")?;
assert_eq!(s.get(b"key")?, Some(b"value".to_vec()));
# Ok::<(), std::io::Error>(())
```

`Store` is `&self` everywhere — clone it freely between threads, the
shards do their own locking. For a persistent file-backed store use
`Config::default().with_persist("/var/lib/myapp")`. To embed as a
read-replica of a server primary (v1.22), see
[`docs/replication.md`](docs/replication.md).

## Resource-adaptive by design

kevy follows one rule about resources: **release performance when there's
room, stay alive when there isn't, gate hard at the edge, and fail loudly —
never silently.** This runs end to end through the engine:

- **Unbounded = full speed.** With `maxmemory = 0` (the default) there is no
  accounting overhead at all — the eviction bookkeeping is compiled past on a
  single not-taken branch. You pay nothing for a limit you don't set.
- **Bounded = graceful eviction.** Set `maxmemory` + a policy (LRU / LFU /
  Random / TTL, 8 in total) and writes evict sampled keys back to **5% below**
  the limit — headroom so the next write doesn't immediately re-enter eviction.
- **Edge = loud refusal, not corruption.** Under `NoEviction` (the default
  policy) a write that would exceed the budget is refused with Redis's classic
  `OOM` error before it runs — an O(1) precheck on the hot path. Only
  memory-*growing* verbs are gated; shrinkers (`DEL`, `LPOP`, `SREM`,
  `EXPIRE`, …) and `FLUSH*` always go through, so you can always recover a full
  instance.
- **Capability degrades, not crashes.** io_uring is probed at startup and
  **falls back to epoll** on older kernels / seccomp sandboxes (force either
  with `KEVY_IO_URING`). The `wasm32` embedded build runs with a host-fed clock
  and reduced surface rather than refusing to build. A non-loopback `--bind`
  **prints a warning** (kevy has no AUTH/TLS) instead of silently exposing you.

The cluster-aware [`ClusterClient`](#cluster-mode-single-node-key-aware-routing)
is the same philosophy on the client: spend the connections to skip the
forwarding hop when load justifies it, stay on the simple single port when it
doesn't.

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

The full command list with valkey-parity notes is in
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md). Durability — AOF,
snapshots, TTL semantics, rewrite/compaction, crash recovery, and the embedded
introspection API — is documented in [`docs/persistence.md`](docs/persistence.md).

## Build & test

```sh
cargo build --workspace --release
cargo test  --workspace
bash bench/run.sh        # portability smoke (docker, no pipeline) — NOT a perf benchmark
bash bench/loopback_c50.sh   # headline perf vs valkey/redis (Linux, host-loopback, pinned)
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
