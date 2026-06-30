# kevy

A Redis-wire-compatible single-DC key–value server in pure Rust.
Zero `crates.io` dependencies. Speaks RESP2, so `redis-cli`,
`valkey-cli`, and every Redis client library work unchanged.

```sh
cargo install kevy
kevy --port 6379 &
redis-cli -p 6379 SET hello world
redis-cli -p 6379 GET hello
```

## What this gives you

- All five Redis data types (String, Hash, List, Set, Sorted Set) plus
  Streams, Pub/Sub, Transactions (`MULTI` / `EXEC` / `WATCH` /
  `UNWATCH`), Blocking pops, and the standard persistence verbs.
- A thread-per-core, shared-nothing engine. On Linux the reactor
  prefers `io_uring` (kernel ≥ 5.19) and falls back to epoll. On
  macOS/BSD the reactor uses kqueue.
- AOF + RDB snapshot persistence. Eight Redis eviction policies.
- 98 commands reply-checked byte-for-byte against valkey 9.1.
- A 768 KB stripped binary that boots into under 5 MB of RSS.

## Install

### crates.io

```sh
cargo install kevy
kevy --port 6379
```

### Pre-built binaries

Attached to every [GitHub Release](https://github.com/goliajp/kevy/releases)
for Linux x86_64, Linux aarch64, and macOS Apple Silicon. Each archive
ships the `kevy` binary, an annotated `kevy.toml.example`, the README,
and both license files; a matching `.sha256` is published alongside.

```sh
TAG=v2.0.20
TARGET=x86_64-unknown-linux-gnu   # or aarch64-unknown-linux-gnu, aarch64-apple-darwin
curl -L "https://github.com/goliajp/kevy/releases/download/$TAG/kevy-$TAG-$TARGET.tar.gz" | tar -xz
sudo install "kevy-$TAG-$TARGET/kevy" /usr/local/bin/kevy
kevy --port 6379
```

### Docker

Multi-arch (`linux/amd64`, `linux/arm64`), published to both registries:

```sh
docker run --rm -p 6379:6379 goliakk/kevy:latest        # Docker Hub
docker run --rm -p 6379:6379 ghcr.io/goliajp/kevy:latest # GHCR
```

Image defaults: `KEVY_BIND=0.0.0.0`, `KEVY_PORT=6379`, `KEVY_DIR=/data`,
`KEVY_AOF=1`. Override with `-e` or by appending flags:

```sh
docker run -d --name kevy -p 6379:6379 -v kevy-data:/data \
  goliakk/kevy:latest --threads 4 --port 7000
```

Docker's default seccomp blocks `io_uring_setup`, so the container runs
on epoll. To enable `io_uring` for benchmarks:

```sh
docker run --rm -p 6379:6379 --security-opt seccomp=unconfined \
  goliakk/kevy:latest
```

## Quick start

### First connection

```sh
kevy --port 6379 &
redis-cli -p 6379 PING                  # → PONG
redis-cli -p 6379 SET foo bar           # → OK
redis-cli -p 6379 GET foo               # → "bar"
redis-cli -p 6379 INFO server | head    # → version, uptime, threads, ...
```

### Configuration precedence

CLI flags → environment variables → TOML file → built-in defaults.

```sh
kevy --bind 0.0.0.0 --port 7000 --threads 4 --dir /var/lib/kevy
# env-var equivalents: KEVY_BIND  KEVY_PORT  KEVY_THREADS  KEVY_DIR  KEVY_AOF

kevy --config /etc/kevy/kevy.toml
```

### TOML config example

```toml
[server]
bind     = "127.0.0.1"
port     = 6379
threads  = 0            # 0 = auto-detect CPU count
data_dir = "/var/lib/kevy"

[persistence]
aof          = true
appendfsync  = "everysec"    # "always" | "everysec" | "no"
auto_aof_rewrite_percentage  = 100
auto_aof_rewrite_min_size    = "64mb"

[memory]
maxmemory          = "1gb"
maxmemory_policy   = "allkeys-lru"
maxmemory_samples  = 5

[metrics]
enabled            = true
bind               = "127.0.0.1"
port               = 9100         # Prometheus scrape: http://host:9100/metrics
```

The full annotated schema is in
[`kevy.toml.example`](kevy.toml.example).

### Unix-domain socket

For same-host clients on Linux, point `KEVY_UNIX_SOCKET` at a
filesystem path. The server dual-binds TCP and UDS with identical
RESP semantics; UDS is materially faster than TCP loopback at every
workload measured. See [`docs/uds.md`](https://github.com/goliajp/kevy/blob/develop/docs/uds.md).

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6379
redis-cli -s /tmp/kevy.sock SET foo bar
```

## Operations

### Graceful shutdown

`SIGTERM` triggers a drain: stop accepting new connections, flush the
AOF, run a final snapshot when persistence is enabled, then exit.
`SIGKILL` skips the drain; on next boot the AOF replay resumes from
the last fsync.

### Metrics

Set `[metrics] enabled = true`. The server exposes a Prometheus-format
endpoint at `http://<metrics-bind>:<metrics-port>/metrics`. Key
exported series:

| Series | Meaning |
|---|---|
| `kevy_connections_active` | Current open client connections |
| `kevy_commands_total{cmd=…}` | Per-command command count |
| `kevy_cmd_latency_ns_bucket{cmd=…}` | Histogram of per-command latency |
| `kevy_keys_total{shard=…}` | Key count per shard |
| `kevy_aof_lag_ms` | AOF write-to-fsync lag |
| `kevy_memory_rss_bytes` | Resident set size |

### Backup and restore

```sh
kevy-cli backup --to ./snapshot-2026-07-01.kbackup   # safe online
kevy-cli restore --from ./snapshot-2026-07-01.kbackup --to /var/lib/kevy
```

`backup` runs against a live server; `restore` writes into a fresh
data directory and the server picks the contents up on the next boot.

### Cluster routing (single node, key-aware)

`--cluster` exposes each shard as a virtual cluster node so stock
cluster-aware clients route writes directly to the owning shard.
Topology is reported via `CLUSTER SLOTS / SHARDS / NODES`; a
wrong-shard key on a cluster port replies `-MOVED`.

```sh
kevy --threads 8 --cluster      # main port 6379, shard ports 6380..6387
redis-cli -c -p 6380 SET foo bar
```

Full guide: [`docs/cluster.md`](https://github.com/goliajp/kevy/blob/develop/docs/cluster.md).

### Replication and failover

A kevy server can run as a **primary** that streams every applied
mutation to N replicas, as a **replica** that mirrors a primary, or
with `kevy-elect` integration as a **quorum-failover** node.

```toml
[replication]
role         = "primary"
listen_port_base = 16004

# or
role         = "replica"
upstream     = "primary.example:16004"
```

`REPLICAOF` / `REPLICAOF NO ONE` / `ROLE` retarget and inspect roles
at runtime. Full server + client recipes in
[`docs/replication.md`](https://github.com/goliajp/kevy/blob/develop/docs/replication.md).

### Lua scripting

`EVAL`, `EVALSHA`, and `SCRIPT LOAD / EXISTS / FLUSH` are backed by
the in-house pure-Rust [`luna`](https://github.com/goliajp/luna)
runtime. Default Lua 5.1 (Redis ecosystem compatibility); opt into
5.2 – 5.5 per script with a `#!lua version=N` shebang. Includes pure-
Rust `cmsgpack` and `cjson` standard libraries.

```sh
redis-cli -p 6379 EVAL "return redis.call('INCR', KEYS[1])" 1 counter
```

Full reference: [`docs/lua.md`](https://github.com/goliajp/kevy/blob/develop/docs/lua.md).

## Client compatibility

The following client libraries are verified end-to-end against a
default `kevy --port 6379` instance, including the script-heavy paths:

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

`redis-cli` and `valkey-cli` both work out of the box.

## What's not supported

| Feature | Why kevy doesn't ship it | Recommended alternative |
|---|---|---|
| AUTH | kevy assumes a trusted network. | Authentication proxy in front of kevy. |
| TLS | Same as AUTH — out of charter. | `stunnel` / `envoy` TLS termination. |
| Multi-database `SELECT` | One keyspace per server. | Run multiple kevy instances on different ports. |
| ACL | Single trust domain. | Authentication proxy. |
| Multi-DC active-active | Single-DC charter. | A KV with explicit multi-region support. |
| Gossip discovery | Topology is declarative. | Operator-managed config. |
| Online resharding | Resharding is offline. | Plan capacity at the cluster boundary. |

The detailed charter rationale lives in
[`MIGRATION-FROM-VALKEY.md`](https://github.com/goliajp/kevy/blob/develop/MIGRATION-FROM-VALKEY.md).

## Reproducing the benchmarks

```sh
bash bench/run.sh              # portability smoke (Docker, no pipeline)
bash bench/loopback_c50.sh     # headline TCP loopback vs valkey/redis
```

Full method and the workload-by-workload table are in
[`bench/REPORT.md`](https://github.com/goliajp/kevy/blob/develop/bench/REPORT.md).

## Library entry-point

`kevy` also exposes a Rust library so the server can be started in-
process:

```rust,no_run
use kevy::{Config, serve};

fn main() -> std::io::Result<()> {
    let cfg = Config::default()
        .with_bind("127.0.0.1")
        .with_port(6379)
        .with_data_dir("/var/lib/kevy");
    serve(cfg)
}
```

For embedding the engine without the network reactor at all, use
[`kevy-embedded`](https://crates.io/crates/kevy-embedded) instead.

## License

Licensed under either of [MIT](../../LICENSE-MIT) or
[Apache-2.0](../../LICENSE-APACHE), at your option.
