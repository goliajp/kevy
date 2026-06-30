# kevy on WebAssembly

`kevy-embedded` and its dependency closure compile to WebAssembly so the same in-process KV engine runs inside browsers, edge runtimes, and WASI hosts.

## When you need this

- **Browser KV** — a fast in-memory key/value cache inside a web app, with the same API surface you use on the server.
- **Cloudflare Workers** (and similar edge runtimes) — an in-isolate hot cache that sits in front of platform-provided durable stores.
- **Embedded WASM caches** — sandboxed plugins inside a larger host (game engines, scripting hosts, serverless containers) that want a Redis-shaped store without dragging in a network stack.
- **Server-side WASI plugins** — long-lived `wasm32-wasip1` modules under `wasmtime` / `wasmer` that need persistence to the host filesystem.

## Core idea

It is the same engine, with two things taken out: the OS clock and the OS threads. `kevy-embedded` pulls in `kevy-store`, `kevy-persist`, `kevy-hash`, `kevy-bytes`, `kevy-map`, and `kevy-resp` — all of which build for `wasm32-unknown-unknown` and `wasm32-wasip1`. The network reactor crates (`kevy-rt`, `kevy-sys`, `kevy-uring`) are deliberately not part of that closure, so the WASM build is clean. Where the engine would normally spawn a TTL reaper thread, it instead exposes a `Store::tick()` you call from the host's event loop, and on the threadless browser target it reads a clock the host feeds in. The data structures, commands, and persistence format are unchanged.

## Worked example

```rust
use kevy_embedded::{Config, Store, set_clock_ns, set_wall_clock_ms};

// 1. Open with the manual reaper so we don't try to spawn a thread.
let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// 2. Use the engine. On wasm32-unknown-unknown feed the clock first;
//    on wasm32-wasip1 and native it's read from the OS for you.
set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
set_wall_clock_ms(now_ms_from_host());

store.set(b"hello", b"world")?;
let v = store.get(b"hello")?;            // Some(b"world".to_vec())
store.set_with_ttl(b"flash", b"x", std::time::Duration::from_millis(500))?;

// 3. Drive eviction from the host loop. On the web you'd schedule this
//    with setInterval / requestAnimationFrame; under WASI it's a plain
//    sleep loop.
loop {
    set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
    set_wall_clock_ms(now_ms_from_host());
    let _stats = store.tick();           // expires due keys
    host_sleep_ms(100);
}
```

The host-side glue is small: a JS `setInterval(() => { mod.tick(now()); }, 100)` for the browser, or a regular `std::thread::sleep` loop under WASI. Everything else — `set`, `get`, `del`, hashes, lists, sorted sets, scripting, AOF — is the same code path you ship on Linux.

## Build matrix

| Target | Cargo command | Notes |
|---|---|---|
| `wasm32-unknown-unknown` (browser) | `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | No threads. No `Instant` / `SystemTime` — host feeds the clock via [`set_clock_ns`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs) and [`set_wall_clock_ms`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs). Persistence is an in-memory directory. |
| `wasm32-unknown-unknown` (Cloudflare Workers) | `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | Same module; use `Date.now()` from the Workers runtime as the clock source. Durable persistence goes through Workers KV bindings on the JS side. |
| `wasm32-wasip1` (server-side WASI) | `cargo build --target wasm32-wasip1 -p kevy-embedded` | Threads still absent, but `Instant` and `SystemTime` work, so no host clock feeding is needed. `std::fs` works against preopened directories (`wasmtime --dir=/data`). |
| Native (`x86_64-*`, `aarch64-*`) | `cargo build -p kevy-embedded` | For reference: spawns a background reaper thread by default; nothing manual to drive. |

See [`crates/kevy-embedded/Cargo.toml`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/Cargo.toml) for the dependency closure and [`crates/kevy-embedded/src/lib.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/src/lib.rs) for the re-exports.

## Differences from native

| Concern | Native | WASM |
|---|---|---|
| TTL reaper | Background thread, auto-spawned | Manual: `Config::with_ttl_reaper_manual()` + host calls `Store::tick()` |
| Clock | OS `Instant` / `SystemTime` | `wasm32-wasip1`: OS. `wasm32-unknown-unknown`: host-fed via `set_clock_ns` / `set_wall_clock_ms` |
| Network server | `kevy-rt` + `kevy-sys` + `kevy-uring` listen on TCP | None of those crates are in the WASM build closure; embed directly via `Store` |
| Persistence | AOF in the directory passed to `with_persist` | `wasm32-wasip1`: same, against a preopened host dir. `wasm32-unknown-unknown`: in-memory directory only (mirror writes out from the host if you want durability) |
| Async runtime | Tokio / std threads in user code | Whatever the host gives you (JS event loop, Workers fetch handler, WASI single-threaded loop) |

## Trade-offs

- **TTL precision tracks your loop cadence.** Keys with a 500 ms TTL only expire on the next `tick()` after the deadline. A 100 ms loop is typical; tighter is fine, looser is fine for cache-style use, but the engine cannot do better than the host gives it.
- **No async runtime is bundled.** kevy-embedded does not pull in `tokio` or `wasm-bindgen-futures`. The host owns the loop; the library exposes synchronous methods that finish in microseconds.
- **No background work means no surprises and no hidden costs**, but it also means a forgotten `tick()` will leave expired keys live and grow memory. Wire the call into the same place you wire your other periodic work.
- **`wasm32-unknown-unknown` durability is not automatic.** Without a filesystem you either run as a pure in-memory cache or mirror writes to a host-side sink (Workers KV, IndexedDB, etc.).

## FAQ

**Does it work in the browser?** Yes. Build for `wasm32-unknown-unknown`, ship the resulting `.wasm` with `wasm-bindgen` or similar bindings, open with `Config::default().with_ttl_reaper_manual()`, and feed the clock from `Date.now()` before each `tick()`. The full command surface — strings, hashes, lists, sets, sorted sets, pub/sub, scripting — works in-process.

**Cloudflare Workers — what's the minimal setup?** Compile `kevy-embedded` for `wasm32-unknown-unknown`, instantiate one `Store` per isolate, and call `tick()` either lazily (before TTL-sensitive reads) or from a scheduled handler. The clock source is `Date.now()` from the Workers runtime. For durability across isolate restarts, mirror writes to Workers KV or D1 from your JS handler; the engine itself stays in-memory.

**How do I persist?** Under `wasm32-wasip1`, call `Config::with_persist("/data")` and launch your module with `wasmtime --dir=/data` (or the equivalent for your runtime). The AOF goes to the preopened directory and replays on the next open. Under `wasm32-unknown-unknown` there is no filesystem, so persistence has to be host-mediated — typically mirroring writes to whatever durable store the platform provides.

**Threads — what about Atomics-enabled WASM?** The default WASM build runs single-threaded, which matches every shipping browser-style target. If your host runtime exposes shared-memory threads (`wasm32-unknown-unknown` with `--target-feature=+atomics,+bulk-memory` plus a thread pool), `Store` is still safe to use, but the background reaper mode is still off — the manual `tick()` model is the supported path, and threads in your code can share a `Store` and call into it concurrently.
