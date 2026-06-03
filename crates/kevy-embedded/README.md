# kevy-embedded

In-process Redis-compatible key–value store — kevy without the network.
Pure Rust, zero `crates.io` dependencies, builds for `wasm32` as well as
native.

```rust
use kevy_embedded::{Store, Config};

let s = Store::open(Config::default())?;
s.set(b"greeting", b"hello")?;
assert_eq!(s.get(b"greeting")?, Some(b"hello".to_vec()));
# Ok::<(), std::io::Error>(())
```

## Install

```sh
cargo add kevy-embedded
```

## When to use

- **Embedded cache** — replace `lru::LruCache` / `moka` / `dashmap` with
  a fully Redis-semantic LRU (or LFU) that speaks all 5 data types.
- **Embedded persistent store** — opt into AOF + snapshot via
  `Config::default().with_persist("./data")`. Restart-safe out of the
  box.
- **WASM / single-threaded apps** — use
  `Config::with_ttl_reaper_manual()` and call `Store::tick()` from your
  own event loop. Full WASM walkthrough (browser / WASI / Cloudflare
  Workers) in [`docs/wasm.md`](https://github.com/goliajp/kevy/blob/develop/docs/wasm.md).

## When NOT to use

- You want a TCP-reachable Redis server → use the [`kevy`](https://crates.io/crates/kevy)
  crate's `serve(...)` entry point or the `goliakk/kevy` Docker image.
  `kevy` server runs the full thread-per-core reactor + cross-shard
  routing.
- You need cross-process concurrency → kevy-embedded is single-process
  (one mutex). For multi-process / multi-host, the network layer is the
  contract — use the server.

## All five Redis data types

```rust
use kevy_embedded::{Store, Config};

let s = Store::open(Config::default())?;

// String
s.set(b"k", b"v")?;
assert_eq!(s.get(b"k")?, Some(b"v".to_vec()));
s.incr(b"counter")?;            // returns 1
s.incr_by(b"counter", 41)?;     // returns 42

// Hash
s.hset(b"user:1", &[(b"name", b"alice"), (b"age", b"30")])?;
assert_eq!(s.hget(b"user:1", b"name")?, Some(b"alice".to_vec()));

// List
s.rpush(b"queue", &[b"a", b"b", b"c"])?;
assert_eq!(s.lpop(b"queue", 1)?, vec![b"a".to_vec()]);

// Set
s.sadd(b"tags", &[b"rust", b"kv", b"embed"])?;
assert_eq!(s.scard(b"tags")?, 3);
assert!(s.smembers(b"tags")?.iter().any(|m| m == b"rust"));

// Sorted set — note the (score, member) tuple order
s.zadd(b"leaderboard", &[(100.0, b"alice"), (200.0, b"bob")])?;
assert_eq!(s.zscore(b"leaderboard", b"bob")?, Some(200.0));
# Ok::<(), std::io::Error>(())
```

## Persistence

`Config::default().with_persist(dir)` enables both snapshot
(`dir/dump-0.rdb`) and AOF (`dir/aof-0.aof`). On `Store::open` the
snapshot loads first, then the AOF replays — a fresh process picks up
exactly where the previous one left off. AOF auto-appends on every
write; fsync policy:

| Policy | Data loss on crash | Throughput |
|---|---|---|
| `Always` | 0 bytes | ~50 % vs `EverySec` |
| `EverySec` (default) | ≤ 1 second | baseline |
| `No` | up to ~30 s (kernel pagecache flush) | slightly faster |

```rust
use kevy_embedded::{Store, Config, AppendFsync};

let s = Store::open(
    Config::default()
        .with_persist("./mydata")
        .with_appendfsync(AppendFsync::Always)   // strict no-loss
)?;
```

`Store::save_snapshot()` runs the equivalent of `SAVE` — dumps a full
snapshot synchronously. `Store::rewrite_aof()` runs the equivalent of
`BGREWRITEAOF` — rebuilds a compact AOF from current in-memory state
and atomically swaps it in. v1.0 is synchronous (blocks the calling
thread); v1.x will incrementalise.

## Eviction

Set a hard memory ceiling via `Config::with_max_memory(bytes)` plus an
`EvictionPolicy`:

```rust
use kevy_embedded::{Store, Config, EvictionPolicy};

let s = Store::open(
    Config::default()
        .with_max_memory(64 * 1024 * 1024)    // 64 MB
        .with_eviction(EvictionPolicy::AllKeysLru)
)?;
```

All 8 Redis policies are supported: `NoEviction`, `AllKeysLru`,
`AllKeysLfu`, `AllKeysRandom`, `VolatileLru`, `VolatileLfu`,
`VolatileRandom`, `VolatileTtl`. LRU/LFU approximation matches Redis
(24-bit clock + sample-based selection with `maxmemory-samples = 5`).

## Thread safety

`Store::set` / `get` / etc. take `&self`. Internally there's **one
`Mutex`** around the keyspace — fine for embedded use, where the
amortised cost is dwarfed by your app's work. **`Store` is `Clone`
(v1.1.0+)**: a clone is a cheap `Arc` bump that reaches the same
underlying keyspace + AOF + reaper + pub/sub bus. The reaper thread is
joined and the AOF is flushed exactly once, when the last clone drops.

```rust
use kevy_embedded::{Store, Config};

let s = Store::open(Config::default())?;
let s2 = s.clone();
std::thread::spawn(move || {
    s2.set(b"from-thread", b"works").unwrap();
});
# Ok::<(), std::io::Error>(())
```

For cross-core scale, use the [`kevy`](https://crates.io/crates/kevy)
server instead — it shards the keyspace across cores with no shared lock.

## In-process pub/sub (v1.1.0+)

```rust
use kevy_embedded::{Store, Config, PubsubFrame};

let s = Store::open(Config::default())?;
let s2 = s.clone();
let mut sub = s.subscribe(&[b"news"]);
let _ack = sub.recv()?;

s2.publish(b"news", b"hello");
match sub.recv()? {
    PubsubFrame::Message { channel, payload } => { /* deliver to your app */ }
    _ => {}
}
# Ok::<(), std::io::Error>(())
```

Channel + pattern subscriptions (`PSUBSCRIBE` glob syntax). Drop the
`Subscription` to unsubscribe from everything atomically. Pair with the
[`kevy-client`](https://crates.io/crates/kevy-client) URL facade to
make the same code work against an in-process bus (`mem://name`) in dev
and a kevy server (`kevy://host:port`) in prod — no scheme branching.

## Migrating from `lru` / `moka` / `dashmap`

| If you had... | kevy-embedded equivalent | Notes |
|---|---|---|
| `lru::LruCache<K, V>` | `Store + with_eviction(AllKeysLru)` | Byte-keys (`&[u8]`); `with_max_memory` instead of count cap |
| `moka::sync::Cache` | `Store + with_eviction(AllKeysLfu)` | LFU matches moka's default expectation |
| `dashmap::DashMap` | `Arc<Store>` | DashMap is concurrent; one Mutex but value is much richer (5 types, persistence) |
| `sled::Db` | `Store + with_persist` | sled is a tree DB; kevy is a hash KV — pick by access pattern |

Versus `redis::Client::open("redis://...")` against a local Redis — you
**lose** zero performance and **gain**:
- No TCP roundtrip (~100 µs each)
- No serialization overhead
- One process to deploy
- No background server to monitor

You **keep** Redis semantics: TTL, eviction, all 5 types, byte-strings.

## Maintenance hooks

For very long-running embedded use, periodically:

```rust
s.tick();           // active TTL reaper — drops expired keys eagerly
s.save_snapshot()?; // RDB-style dump for restart speed
s.rewrite_aof()?;   // compact AOF, drops redundant writes
```

If you're in `Config::with_ttl_reaper_manual()` mode (WASM /
single-threaded), `tick()` is the only way TTL'd keys get reaped between
accesses.

## Examples

In the repo: [`examples/embedded.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/embedded.rs)
— minimal CRUD; [`examples/embedded-cache.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/embedded-cache.rs)
— hard-cap LRU cache.

## Dependencies

Zero `crates.io` dependencies. Only `kevy-store` (keyspace) +
`kevy-persist` (snapshot / AOF). The whole network layer
(`kevy-rt`, `kevy-sys`, `kevy-uring`) is intentionally NOT pulled in,
so kevy-embedded compiles for any target `kevy-store + kevy-persist`
compile for — including `wasm32-unknown-unknown` and `wasm32-wasip1`.

## License

MIT OR Apache-2.0, at your option.
