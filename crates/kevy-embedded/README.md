# kevy-embedded

In-process Redis-compatible key–value store — kevy without the network.

```rust
use kevy_embedded::{Store, Config};

let s = Store::open(Config::default())?;
s.set(b"greeting", b"hello")?;
assert_eq!(s.get(b"greeting")?, Some(b"hello".to_vec()));
```

## When to use

- **Embedded cache** — replace `lru::LruCache` / `moka` with a fully
  Redis-semantic LRU (or LFU) that speaks all 5 data types.
- **Embedded persistent store** — opt into AOF + snapshot via
  `Config::with_persist("./data")`. Restart-safe out of the box.
- **WASM / single-threaded apps** — use
  `Config::with_ttl_reaper_manual()` and call `Store::tick()` from your
  own event loop. Full WASM walkthrough (browser / WASI / Cloudflare
  Workers): [`../../docs/wasm.md`](../../docs/wasm.md).

## When NOT to use

- You want a TCP-reachable Redis server → use the `kevy` crate's
  `serve(...)` entry point instead. `kevy` runs the full thread-per-core
  reactor + cross-shard routing.
- You need cross-process concurrency → kevy-embedded is single-process
  (one mutex). For multi-process, the network layer is the contract.

## Persistence

`with_persist(dir)` enables both snapshot (`dump-0.rdb`) and AOF
(`aof-0.aof`). On `open` the snapshot loads first, then the AOF
replays — so a fresh process picks up exactly where the previous one
left off. AOF auto-appends on every write; fsync policy follows
`Config::with_appendfsync(...)`:

| Policy | Data loss on crash | Throughput |
|---|---|---|
| `Always` | 0 bytes | ~50 % vs `EverySec` |
| `EverySec` (default) | ≤ 1 second | baseline |
| `No` | up to ~30 s (kernel pagecache flush) | slightly faster |

`Store::rewrite_aof()` runs the equivalent of `BGREWRITEAOF` —
rebuilds a compact AOF from current in-memory state and atomically
swaps it in. v1.0 is synchronous (blocks the calling thread); v1.x
will incrementalise.

## Eviction

Set a hard memory ceiling via `Config::with_max_memory(bytes)` plus an
`EvictionPolicy` (all 8 Redis policies — `NoEviction` / `AllKeysLru` /
`AllKeysLfu` / `AllKeysRandom` / `VolatileLru` / `VolatileLfu` /
`VolatileRandom` / `VolatileTtl`). LRU/LFU approximation matches Redis
(24-bit clock + sample-based selection with `maxmemory-samples = 5`).

## Examples

- [`examples/embedded.rs`](examples/embedded.rs) — minimal CRUD.
- [`examples/embedded-cache.rs`](examples/embedded-cache.rs) —
  hard-cap LRU cache.

## Charter

Zero crates.io dependencies. Only `kevy-store` (keyspace) +
`kevy-persist` (snapshot / AOF). The whole network layer
(`kevy-rt`, `kevy-sys`, `kevy-uring`) is intentionally NOT pulled in,
so kevy-embedded compiles for any target `kevy-store + kevy-persist`
compile for — including `wasm32-unknown-unknown` and
`wasm32-wasip1`.
