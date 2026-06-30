# kevy-embedded

The kevy key–value engine as a Rust library — same data structures, same
commands, no network. Drop it into a binary and call `Store` directly.

- Pure Rust, zero `crates.io` dependencies.
- All five Redis data types plus bitmaps, pub/sub, TTL, and three
  transaction shapes.
- Snapshot + append-only-file persistence, eight eviction policies.
- Builds for `wasm32-unknown-unknown` and `wasm32-wasip1`.

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;
store.set(b"greeting", b"hello")?;
assert_eq!(store.get(b"greeting")?, Some(b"hello".to_vec()));
# Ok::<(), std::io::Error>(())
```

## Install

```sh
cargo add kevy-embedded
```

## When to use

- **In-process cache or store.** A Redis-shaped LRU/LFU that handles
  bytes, hashes, lists, sets, sorted sets, bitmaps, and TTL.
- **Embedded persistent KV.** `Config::default().with_persist("./data")`
  enables snapshot + AOF and the next process boot resumes where the
  last one stopped.
- **WebAssembly target.** No threads, no OS sockets — set
  `Config::with_ttl_reaper_manual()` and call `Store::tick()` from your
  event loop. Walkthrough in [`docs/wasm.md`](https://github.com/goliajp/kevy/blob/develop/docs/wasm.md).

## When NOT to use

- **Cross-process access.** `kevy-embedded` is single-process. Use the
  [`kevy`](https://crates.io/crates/kevy) server when more than one
  process needs to share state.
- **Distributed consistency or replication you control.** Embed-as-
  read-replica exists (see further down) but the writer is still a
  single kevy server. For multi-writer, multi-region, transactional
  consistency, pick a real distributed database.

## Quick start

### Strings

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;

store.set(b"k", b"v")?;
assert_eq!(store.get(b"k")?, Some(b"v".to_vec()));

store.incr(b"counter")?;
store.incr_by(b"counter", 41)?;
assert_eq!(store.get(b"counter")?, Some(b"42".to_vec()));

store.append(b"log", b"hello")?;
store.append(b"log", b" world")?;
assert_eq!(store.strlen(b"log")?, 11);
# Ok::<(), std::io::Error>(())
```

Atomic single-call helpers: `getset`, `getdel`, `setnx`, `setrange`,
`getrange`, `decr`, `decr_by`, `incrbyfloat`, `mset`, `mget`.

### Hashes

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;

store.hset(b"user:1", &[(&b"name"[..], &b"alice"[..]),
                       (&b"age"[..],  &b"30"[..])])?;

assert_eq!(store.hget(b"user:1", b"name")?, Some(b"alice".to_vec()));
assert_eq!(store.hlen(b"user:1")?, 2);
assert!(store.hexists(b"user:1", b"name")?);

let all: Vec<(Vec<u8>, Vec<u8>)> = store.hgetall(b"user:1")?;
let some = store.hmget(b"user:1", &[&b"name"[..], &b"age"[..]])?;

store.hincrby(b"user:1", b"age", 1)?;
store.hsetnx(b"user:1", b"created_at", b"2026-01-01")?;
# Ok::<(), std::io::Error>(())
```

### Lists

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;

store.rpush(b"queue", &[&b"a"[..], &b"b"[..], &b"c"[..]])?;
assert_eq!(store.llen(b"queue")?, 3);

let head = store.lpop(b"queue", 1)?;
assert_eq!(head, vec![b"a".to_vec()]);

let window: Vec<Vec<u8>> = store.lrange(b"queue", 0, -1)?;
assert_eq!(window, vec![b"b".to_vec(), b"c".to_vec()]);
# Ok::<(), std::io::Error>(())
```

Plus `lindex`, `linsert`, `lrem`, `lset`, `ltrim`, and the blocking
variants when running inside a server.

### Sets

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;

store.sadd(b"tags", &[&b"rust"[..], &b"kv"[..], &b"embed"[..]])?;
assert_eq!(store.scard(b"tags")?, 3);
assert!(store.sismember(b"tags", b"rust")?);

store.sadd(b"a", &[&b"x"[..], &b"y"[..]])?;
store.sadd(b"b", &[&b"y"[..], &b"z"[..]])?;
let inter = store.sinter(&[&b"a"[..], &b"b"[..]])?;
assert_eq!(inter, vec![b"y".to_vec()]);
# Ok::<(), std::io::Error>(())
```

### Sorted sets

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;

// Note: (score, member) tuple order.
store.zadd(b"leaderboard", &[(100.0, &b"alice"[..]),
                             (200.0, &b"bob"[..])])?;

assert_eq!(store.zscore(b"leaderboard", b"bob")?, Some(200.0));
assert_eq!(store.zrank(b"leaderboard", b"alice")?, Some(0));

let top: Vec<(Vec<u8>, f64)> = store.zrevrange(b"leaderboard", 0, 9)?;
assert_eq!(top[0].0, b"bob");

store.zincrby(b"leaderboard", 50.0, b"alice")?;
# Ok::<(), std::io::Error>(())
```

Range queries: `zrange`, `zrevrange`, `zrange_by_score`,
`zrev_range_by_score`, `zcount`, `zpopmin`, `zremrangebyrank`,
`zremrangebyscore`.

### Bitmaps

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;

store.setbit(b"bloom", 0xff, 1)?;
assert_eq!(store.getbit(b"bloom", 0xff)?, 1);
assert_eq!(store.bitcount(b"bloom", None)?, 1);

store.setbit(b"a", 0, 1)?;
store.setbit(b"a", 7, 1)?;
store.setbit(b"b", 0, 1)?;
store.bitop("and", b"dest", &[&b"a"[..], &b"b"[..]])?;
# Ok::<(), std::io::Error>(())
```

Plus `bitpos`, `getrange`, and `setrange` for byte-aligned slice work.

### TTL

```rust
use kevy_embedded::{Config, Store};
use std::time::Duration;

let store = Store::open(Config::default().without_aof())?;

store.set(b"session", b"value")?;
store.expire(b"session", Duration::from_secs(3600))?;
store.pexpire(b"cache:k", 30_000)?;            // 30 seconds in ms
store.expireat(b"abs", 1_900_000_000)?;        // absolute unix-second

assert!(store.ttl_ms(b"session") > 0);
let ttl_s = store.ttl_secs(b"session");         // -1 no TTL, -2 absent

// Atomic get + (re)set TTL in one call.
let val = store.getex(b"session", Duration::from_secs(60))?;
# Ok::<(), std::io::Error>(())
```

### In-process pub/sub

```rust
use kevy_embedded::{Config, PubsubFrame, Store};

let store = Store::open(Config::default().without_aof())?;
let publisher = store.clone();
let mut sub = store.subscribe(&[&b"news"[..]]);
let _ack = sub.recv()?;                         // drain the SUBSCRIBE ack

publisher.publish(b"news", b"hello");
match sub.recv()? {
    PubsubFrame::Message { channel, payload } => {
        assert_eq!(channel, b"news");
        assert_eq!(payload, b"hello");
    }
    _ => unreachable!(),
}
# Ok::<(), std::io::Error>(())
```

Channel and pattern (`PSUBSCRIBE`-style glob) subscriptions are both
supported. Dropping the `Subscription` unsubscribes from every channel
atomically.

### Cursor-based scan

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;
for i in 0..1_000u32 {
    store.set(format!("user:{i}").as_bytes(), b"x")?;
}

let user_keys: Vec<Vec<u8>> = store
    .keys_iter(Some(b"user:*"))
    .collect();
assert_eq!(user_keys.len(), 1_000);
# Ok::<(), std::io::Error>(())
```

The `keys_iter`, `hash_iter`, and `zset_iter` wrappers turn the raw
`SCAN` / `HSCAN` / `ZSCAN` cursors into ordinary Rust iterators.

## Three transaction shapes — which to pick

`kevy-embedded` exposes three commit shapes. They differ on what they
guarantee, not on what they let you write inside the closure.

| Shape | Atomicity | Cross-shard | Fsync per commit | Use when |
|---|---|---|---|---|
| `Store::atomic` | All-or-nothing | Single shard (all keys share a hashtag) | One | One key or one hashtag group. Highest throughput. |
| `Store::atomic_all_shards` | All-or-nothing | All shards | One | A read-modify-write that spans multiple unrelated keys. |
| `Store::pipeline` | Per-op | Any | One per batch | High-throughput write streams where the app is fine with per-op failure. |

### `atomic` — single-shard atomic closure

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;

// Both keys share the {user:42} hashtag → same shard.
let result = store.atomic(b"{user:42}:counter", |s| {
    let n = s.incr(b"{user:42}:counter")?;
    if n == 1 {
        s.set(b"{user:42}:seen", b"first")?;
    }
    Ok(n)
})?;
assert_eq!(result, 1);
# Ok::<(), std::io::Error>(())
```

### `atomic_all_shards` — multi-shard atomic closure

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;

store.atomic_all_shards(|s| {
    let count = s.incr(b"global:counter")?;
    s.set(b"users:last_id", count.to_string().as_bytes())?;
    s.hset(b"users:by_id", &[(count.to_string().as_bytes(),
                              b"new")])?;
    Ok(())
})?;
# Ok::<(), std::io::Error>(())
```

Acquires every shard lock in deterministic order, so it is heavier than
`atomic` and should be used only when the closure genuinely needs more
than one shard.

### `pipeline` — non-atomic batched writes

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;

let mut p = store.pipeline();
for i in 0..1000 {
    p.set(format!("k{i}").as_bytes(), b"v");
}
let replies = p.execute()?;        // one fsync, 1000 entries
assert_eq!(replies.len(), 1000);
# Ok::<(), std::io::Error>(())
```

Each command commits independently, so a single command failing does
not roll back its neighbours. One fsync at the end of `execute()`
amortises the cost across the batch.

## Persistence

`Config::default().with_persist(dir)` enables both snapshot and AOF.
`Store::open` first loads the snapshot, then replays the AOF, so a
fresh process resumes exactly where the previous one left off.

```rust
use kevy_embedded::{AppendFsync, Config, Store};

let store = Store::open(
    Config::default()
        .with_persist("./mydata")
        .with_appendfsync(AppendFsync::EverySec)
)?;
# Ok::<(), std::io::Error>(())
```

| `AppendFsync` | Max data loss on crash | Throughput vs `EverySec` |
|---|---|---|
| `Always` | 0 bytes | ~50% |
| `EverySec` (default) | ≤ 1 second | baseline |
| `No` | up to ~30 s (kernel pagecache flush) | slightly faster |

Compaction:

- `Store::save_snapshot()` writes a full snapshot synchronously
  (equivalent of `SAVE`).
- `Store::rewrite_aof()` rebuilds a compact AOF from current state and
  atomically swaps it in (equivalent of `BGREWRITEAOF`).

## Eviction

```rust
use kevy_embedded::{Config, EvictionPolicy, Store};

let store = Store::open(
    Config::default()
        .with_max_memory(64 * 1024 * 1024)         // 64 MB
        .with_eviction(EvictionPolicy::AllKeysLru)
)?;
# Ok::<(), std::io::Error>(())
```

All eight Redis policies are supported: `NoEviction` (default),
`AllKeysLru`, `AllKeysLfu`, `AllKeysRandom`, `VolatileLru`,
`VolatileLfu`, `VolatileRandom`, `VolatileTtl`. LRU and LFU use Redis-
compatible 24-bit clock + sample-based selection.

Under `NoEviction` a write that would exceed `max_memory` returns the
standard Redis `OOM` error before it runs. Shrinking verbs (`DEL`,
`LPOP`, `SREM`, `EXPIRE`, `FLUSH*`) always succeed so the instance is
always recoverable.

## Thread safety

`Store` methods take `&self`, and `Store` is `Clone` — each clone is an
`Arc` bump that reaches the same keyspace, AOF, reaper, and pub/sub
bus. The reaper joins and the AOF flushes exactly once when the last
clone drops.

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;
let s2 = store.clone();
std::thread::spawn(move || {
    s2.set(b"from-thread", b"works").unwrap();
});
# Ok::<(), std::io::Error>(())
```

For multi-core scale where a single mutex would dominate, use the
[`kevy`](https://crates.io/crates/kevy) server, which shards the
keyspace across cores with no shared lock.

## Join a kevy server cluster from your own process

Three deployment shapes, all backed by the same `Store` API.

### Pure embed — no network

The default. Reads and writes hit the in-process keyspace.

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().with_persist("./data"))?;
store.set(b"k", b"v")?;
# Ok::<(), std::io::Error>(())
```

### Embed as a read replica

Subscribe to a kevy server primary's replication stream. Every applied
mutation flows into the in-process `Store` over RESP. Local reads pay
zero network round-trip; local writes return `READONLY`.

```rust
use kevy_embedded::Store;

let store = Store::open_replica("primary.internal:16004")?;

let v: Option<Vec<u8>> = store.get(b"hot-key")?;
assert!(store.is_replica());
// store.set(b"k", b"v")  →  Err(READONLY)
# Ok::<(), std::io::Error>(())
```

Use the tunable form when you need a stable replica id (so the primary
reuses your backlog after a quick restart) or a custom reconnect
window:

```rust
use kevy_embedded::{Config, Store};
use std::time::Duration;

let store = Store::open(
    Config::default()
        .with_replica_upstream("primary.internal:16004")
        .with_replica_id("app-billing-pod-7")
        .with_replica_reconnect(Duration::from_millis(50),
                                Duration::from_secs(5))
)?;
# Ok::<(), std::io::Error>(())
```

### Embed as a scoped writer

The cluster declares per-prefix writer ownership on the server side
(`[cluster] scopes = "app:billing:=embed-a"` in the server's TOML). An
embed process that owns a prefix writes locally, while wrong-prefix
writes anywhere in the cluster are redirected with `-MISDIRECTED writer
is <host:port>`. See [`docs/cluster.md`](https://github.com/goliajp/kevy/blob/develop/docs/cluster.md)
for the server-side TOML and the `MOVE-SCOPE` migration protocol.

## URL facade — same code switches between embed and server

`kevy-client` accepts both `mem://` (in-process via `kevy-embedded`)
and `kevy://host:port` (TCP):

```rust,no_run
use kevy_client::Connection;

let url = std::env::var("KEVY_URL")
    .unwrap_or_else(|_| "mem://app".into());
let mut conn = Connection::open(&url)?;
conn.set(b"k", b"v")?;
# Ok::<(), std::io::Error>(())
```

A single Rust binary can run as a server, as a pure embedded library,
or as a hybrid (embed-as-replica + remote primary) — the calling code
never branches on transport.

## Out of scope

`kevy-embedded` deliberately omits:

- **Multi-database `SELECT`.** Single keyspace per `Store`.
- **AUTH and ACL.** Single trust domain — the calling process.
- **`EVAL` / `SCRIPT`.** The Lua scripting bridge ships in the
  `kevy` server crate.
- **Cluster mode commands.** `CLUSTER SLOTS / SHARDS / NODES` belong on
  the server side; the embedded library is single-process.

## Maintenance hooks

For very long-running embedded use:

```rust,no_run
# use kevy_embedded::{Config, Store};
# let store = Store::open(Config::default().without_aof())?;
store.tick();                  // active TTL reaper
store.save_snapshot()?;        // RDB-style dump for restart speed
store.rewrite_aof()?;          // compact AOF, drop redundant writes
# Ok::<(), std::io::Error>(())
```

When running under `Config::with_ttl_reaper_manual()` (WASM, single-
threaded host), `tick()` is the only path through which expired keys
are reaped.

## Examples in the repository

- [`examples/embedded.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/embedded.rs)
  — minimum-viable CRUD.
- [`examples/embedded-cache.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/embedded-cache.rs)
  — hard-cap LRU cache.
- [`examples/embed_vs_server.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/embed_vs_server.rs)
  — same Rust caller against in-process, kevy server, valkey, redis.

## Dependencies

Zero `crates.io` dependencies. The only crates pulled in are kevy's
own `kevy-store`, `kevy-persist`, `kevy-hash`, and `kevy-replicate` —
all path-deps inside the workspace. The network reactor crates
(`kevy-rt`, `kevy-sys`, `kevy-uring`) are intentionally not pulled,
so `kevy-embedded` compiles for any target `kevy-store + kevy-persist`
compile for, including `wasm32-unknown-unknown` and `wasm32-wasip1`.

## License

MIT OR Apache-2.0, at your option.
