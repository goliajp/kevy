# kevy

A pure-Rust, **zero-dependency**, Redis-compatible key–value server, built to run
as fast as the hardware allows.

- **Pure Rust, 0 deps.** No crates.io dependencies — only `std` and kevy's own
  crates. The *only* C touched is the unavoidable OS-boundary libc (sockets,
  epoll/kqueue, pipes), bound by hand with `unsafe extern "C"` in one crate
  (`kevy-sys`). Every algorithm and data structure is written in Rust.
- **Thread-per-core, shared-nothing.** One reactor + one keyspace shard per core,
  no locks on the hot path; cores coordinate only by message passing.
- **Redis wire protocol** (RESP2/RESP3 replies), so `redis-cli` / `valkey-cli`
  and any Redis client can talk to it.
- **Durable**: RDB-style snapshots + an append-only log (AOF).
- Rust 2024 edition, toolchain 1.95. Author: **GOLIA K.K.**

> Performance north star: the **hardware ceiling** (ultimately disk-I/O
> bandwidth). Beating valkey is the floor, not the goal.

## Crates

| crate | role |
|-------|------|
| `kevy-sys` | the sole libc boundary: sockets, `Poller` (kqueue/epoll), self-pipe `Waker`, `SO_REUSEPORT` |
| `kevy-resp` | RESP2/3 wire codec |
| `kevy-store` | the keyspace: values, lazy expiry, snapshot iteration |
| `kevy-net` | a single-threaded event-driven reactor (engine-agnostic `Service`) |
| `kevy-rt` | the thread-per-core shared-nothing runtime (routing, cross-core messaging, reply ordering, fan-out) |
| `kevy-persist` | RDB-style snapshots + AOF |
| `kevy` | the server binary + command set |

## Commands

All five core Redis data types are supported (backed by modern structures, not
Redis's legacy encodings):

- **Connection/admin** — `PING ECHO HELLO QUIT COMMAND CONFIG SAVE BGSAVE`
- **Keys** — `DEL EXISTS EXPIRE PEXPIRE TTL PTTL PERSIST TYPE DBSIZE FLUSHDB FLUSHALL KEYS SCAN RANDOMKEY`
- **String** — `SET`(`EX`/`PX`/`NX`/`XX`)`GET MSET MGET GETSET GETDEL SETNX SETEX PSETEX APPEND STRLEN INCR DECR INCRBY DECRBY INCRBYFLOAT`
- **Hash** — `HSET HSETNX HGET HDEL HEXISTS HLEN HINCRBY HKEYS HVALS HGETALL HMGET`
- **List** — `LPUSH RPUSH LPOP RPOP LLEN LINDEX LRANGE LSET LREM LTRIM`
- **Set** — `SADD SREM SCARD SISMEMBER SMEMBERS SPOP SRANDMEMBER SINTER SUNION SDIFF`
- **Sorted set** — `ZADD ZSCORE ZCARD ZREM ZRANK ZINCRBY ZRANGE ZRANGEBYSCORE ZCOUNT`
- **Pub/sub** — `SUBSCRIBE UNSUBSCRIBE PUBLISH`
- **Transactions** — `MULTI EXEC DISCARD`

Wrong-type access returns `WRONGTYPE`, as in Redis. Multi-key commands
(`MSET`/`MGET`/`SINTER`/…) and pub/sub work across the per-core shards.

## Build & run

```sh
cargo build --release
cargo run -p kevy --bin kevy -- --port 6379
# flags: --bind A.B.C.D  --port N  --threads N  --dir PATH  --no-aof
# env:   KEVY_BIND KEVY_PORT KEVY_THREADS KEVY_DIR KEVY_AOF
```

Then, with any Redis client:

```sh
redis-cli -p 6379 set foo bar
redis-cli -p 6379 get foo
```

## Test

```sh
cargo test --workspace
```

## Benchmark vs valkey 9.1

Both servers run in Docker on the same Linux host (server cores isolated from the
load generator), measured by the neutral `valkey-benchmark`:

```sh
bash bench/run.sh
```

Current results (in-memory, isolated cores) live in [`bench/REPORT.md`](bench/REPORT.md):
kevy **beats valkey on single-connection** throughput (SET/GET ~1.2–1.5×) and
reaches ~0.8× at 50 connections — the remaining gap is being closed in the perf
pass (lock-free cross-core rings, leaner per-command path, io_uring).

## Status

**Single-machine** Redis-compatible KV. Shipped: all five data types, RESP2/3,
thread-per-core runtime, RDB snapshots + AOF, and a valkey benchmark. Perf:
beats valkey single-connection, ~0.9× at 50 connections (see `bench/REPORT.md`).

Refinements still TODO: `WATCH` (optimistic CAS), pattern pub/sub
(`PSUBSCRIBE`), `RENAME` across shards, incremental `SCAN` cursor (currently a
single full pass), and true RESP3 reply encoding (`HELLO` reports proto 2).

Out of scope (single-machine by design): replication, cluster, scripting, auth.

## License

License is not yet declared (the crates are not published). To be decided by
GOLIA K.K.
