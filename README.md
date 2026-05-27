# kevy

A pure-Rust, **zero-dependency**, Redis-compatible key–value server,
built to run as fast as the hardware allows.

- **0 crates.io deps.** Only `std` + kevy's own crates. The only C
  touched is the unavoidable OS-boundary libc (sockets, epoll/io_uring,
  pipes, mmap), bound by hand with `unsafe extern "C"` in one crate
  (`kevy-sys`). Every algorithm and data structure is written in Rust.
- **Thread-per-core, shared-nothing.** One reactor + one keyspace shard
  per core, no locks on the hot path; cores coordinate by message
  passing.
- **Redis wire protocol** (RESP2). `redis-cli`, `valkey-cli`, and every
  Redis client library talks to kevy out of the box —
  [94-cmd parity vs valkey 9.1](MIGRATION-FROM-VALKEY.md).
- **Durable.** Snapshots + append-only file (AOF), `appendfsync` =
  `always` / `everysec` (default) / `no` matching Redis semantics.
- **Configurable via TOML or env/CLI** — see [`kevy.toml.example`](crates/kevy/kevy.toml.example).

## Performance

Beating valkey 9.1 is the floor, not the goal. Current numbers
(`lx64` metal, server cores 0-9, isolated client cores):

| metric            | kevy (io_uring) | valkey 9.1 (io-threads) | ratio  |
|-------------------|----------------:|------------------------:|-------:|
| **-c50 SET / sec**| **4.0 M**       | 1.5 M                   | **2.67×** |
| **-c50 GET / sec**| **4.0 M**       | 1.7 M                   | **2.33×** |
| -c1  SET / sec    |          88 k   |            58 k         | 1.52×  |
| -c1  GET / sec    |          80 k   |            65 k         | 1.25×  |

vs liburing 2.9 (the C reference for io_uring):
**kevy-uring 148 ns nop-round-trip vs liburing 152 ns** — at the
Linux kernel floor.

Per-stone vs best open-source competitor (kevy-bytes, kevy-hash,
kevy-map, kevy-resp, kevy-ring): 8 / 8 at noise-floor parity or
better. See [`perfs/baseline/2026-05-27/V1.1-DEEP-POLISH-CLOSE.md`](perfs/baseline/2026-05-27/V1.1-DEEP-POLISH-CLOSE.md)
for the per-workload breakdown.

## Target scenarios (v1.0)

kevy v1.0 is prod-ready for these 4 use cases:

1. **Local dev** — `cargo run -p kevy` + your favourite redis client
2. **docker-compose internal** — `KEVY_BIND=0.0.0.0` inside the network,
   trust boundary is the docker network itself (kevy has no AUTH/TLS yet
   — see [`.claude/scope-decisions.md`](.claude/scope-decisions.md))
3. **Embedded library** — drop the [`kevy-store`](crates/kevy-store)
   crate into your app, no network, no reactor
4. **Cache** — fronted by a real DB, kevy holds hot data with TTL +
   `maxmemory` + LRU/LFU eviction

Out of scope: replication, cluster, AUTH/TLS, public-internet exposure.
For HA/multi-host go via k8s StatefulSet or sidecar proxy patterns.

## Quick start

### As a server

```sh
# Build + run with all defaults (loopback only, AOF on, port 6004)
cargo run -p kevy --bin kevy --release

# Or use a TOML config file
cp crates/kevy/kevy.toml.example ./kevy.toml
$EDITOR kevy.toml
cargo run -p kevy --bin kevy --release -- --config ./kevy.toml

# Then with any Redis client:
redis-cli -p 6004 SET foo bar
redis-cli -p 6004 GET foo
```

CLI overrides take precedence over env vars over the TOML file:

```sh
kevy --bind 0.0.0.0 --port 7000 --threads 4 --dir /var/lib/kevy
# Equivalent env: KEVY_BIND, KEVY_PORT, KEVY_THREADS, KEVY_DIR, KEVY_AOF
```

### As an embedded library

```rust
// Cargo.toml: kevy-store = "0.1"
use kevy_store::Store;

let mut s = Store::default();
s.set(b"key".to_vec(), b"value".to_vec(), None, false, false);
assert_eq!(s.get(b"key").unwrap().unwrap(), b"value");
```

(Polished `kevy-embedded` API + `Store::with_persist(path)` constructor
+ WASM browser example land in v1.0 Wave 3.)

## Configuration

```toml
# kevy.toml — see crates/kevy/kevy.toml.example for full annotated schema
[server]
bind     = "127.0.0.1"
port     = 6004
threads  = 0           # 0 = auto (CPU count)
data_dir = "."

[persistence]
aof          = true
appendfsync  = "everysec"   # "always" | "everysec" | "no"

[memory]
maxmemory        = 0                  # 0 = unlimited; or "256mb" / "2gb"
maxmemory_policy = "noeviction"       # 8 Redis policies supported
```

Precedence: CLI flags > env vars > TOML file > built-in defaults.
Auto-detect search: `$KEVY_DIR/kevy.toml` → `./kevy.toml` → `/etc/kevy/kevy.toml`.

## Crates

8 stones (published to crates.io) + 1 cement (kevy-sys, bundled with
the server binary):

| crate | role |
|-------|------|
| [`kevy-bytes`](crates/kevy-bytes) | SmallBytes — owned byte string with inline-or-heap SSO |
| [`kevy-hash`](crates/kevy-hash) | fast non-cryptographic hash for single-trust-domain keyspaces |
| [`kevy-map`](crates/kevy-map) | Swiss-table hashmap with SIMD group scan + branchless mirror writes |
| [`kevy-resp`](crates/kevy-resp) | zero-alloc RESP2/3 parser, ~9× faster than redis-rs's |
| [`kevy-ring`](crates/kevy-ring) | bounded lock-free SPSC queue with cached cursors |
| [`kevy-madvise`](crates/kevy-madvise) | Linux `MADV_HUGEPAGE` wrapper, no-op elsewhere |
| [`kevy-uring`](crates/kevy-uring) | pure-Rust io_uring bindings, no liburing |
| [`kevy-resp-client`](crates/kevy-resp-client) | blocking RESP2 client |
| [`kevy-config`](crates/kevy-config) | TOML subset parser + config schema |
| `kevy-sys` | (cement) the sole libc boundary; ships with `kevy` |
| `kevy-store` / `kevy-rt` / `kevy-persist` | server-side keyspace, runtime, persistence |
| `kevy-cli` | redis-cli-style client (works against any RESP2 server) |
| `kevy` | the server binary |

## Commands (94-cmd valkey parity)

All five Redis data types implemented with **modern data structures**,
not Redis's legacy encodings.

- **Connection** — `PING ECHO HELLO QUIT COMMAND`
- **Keys** — `DEL EXISTS EXPIRE PEXPIRE TTL PTTL PERSIST TYPE DBSIZE FLUSHDB FLUSHALL KEYS SCAN RANDOMKEY`
- **String** — `SET GET MSET MGET GETSET GETDEL SETNX SETEX PSETEX APPEND STRLEN INCR DECR INCRBY DECRBY INCRBYFLOAT`
- **Hash** — `HSET HSETNX HGET HDEL HEXISTS HLEN HINCRBY HKEYS HVALS HGETALL HMGET`
- **List** — `LPUSH RPUSH LPOP RPOP LLEN LINDEX LRANGE LSET LREM LTRIM`
- **Set** — `SADD SREM SCARD SISMEMBER SMEMBERS SPOP SRANDMEMBER SINTER SUNION SDIFF`
- **Sorted set** — `ZADD ZSCORE ZCARD ZREM ZRANK ZINCRBY ZRANGE ZRANGEBYSCORE ZCOUNT`
- **Pub/sub** — `SUBSCRIBE UNSUBSCRIBE PUBLISH`
- **Transactions** — `MULTI EXEC DISCARD`
- **Persistence** — `SAVE BGSAVE` (`BGREWRITEAOF` in Wave 2)
- **Operations** — `INFO CLUSTER DEBUG WAIT SHUTDOWN` (`CLIENT *` + full
  `CONFIG GET/SET/REWRITE` in Wave 1 follow-up)

`WRONGTYPE` returns as in Redis. Multi-key commands (`MSET` / `MGET` /
`SINTER` / `SUNION` / `SDIFF`) and pub/sub work across the per-core shards.

## Build & test

```sh
cargo build --workspace --release
cargo test  --workspace
cargo bench  # bench/run.sh — full vs-valkey comparison on Linux
```

Stable Rust 1.95, Rust 2024 edition. Builds on `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, `*-apple-darwin`. Most stones also build on
`wasm32-unknown-unknown` (full wasm story lands in v1.0 Wave 3).

## v1.0 roadmap status

| Wave | Scope | Status |
|---|---|---|
| Wave 1 — config + ops + docs | `kevy-config` crate · INFO/CLUSTER/DEBUG/WAIT/SHUTDOWN · top-level README · MIGRATION doc | **in progress** |
| Wave 2 —防 OOM + 防数据丢 | maxmemory + 8 eviction policies · TTL reaper · BGREWRITEAOF · crash-safe verify | not started |
| Wave 3 — embedded + WASM + 发布 | `kevy-embedded` crate · 32-bit pointer port · WASM examples · GitHub Actions CI · v1.0.0-rc1 tag | not started |

Full v1.0 plan: [`V1.0-BOUNDARY.md`](V1.0-BOUNDARY.md).
Project-wide scope decisions (what's permanently OUT): [`.claude/scope-decisions.md`](.claude/scope-decisions.md).

## License

MIT OR Apache-2.0, at your option. © 2026 GOLIA K.K.
