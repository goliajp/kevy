# kevy on WebAssembly

`kevy-embedded` (the in-process variant of kevy — see
[`crates/kevy-embedded/README.md`](../crates/kevy-embedded/README.md)) **compiles**
to WebAssembly, and its **core in-memory KV runs today** on
`wasm32-unknown-unknown` (`set` / `get` / `dbsize` — verified end-to-end in Node;
see [`examples/wasm-kv/`](../examples/wasm-kv)). The full `kevy` server
(`kevy-rt`, `kevy-sys`) does **not** target wasm — it needs sockets, threads,
and OS pollers that WASM runtimes don't expose.

> ⚠️ **Runtime gap (tracked).** Any operation that touches the TTL clock —
> `set_with_ttl`, `PEXPIRE`/`PTTL`, the reaper `tick`, and even `DEL` (it
> reaps-before-delete) — **panics (`unreachable`) on `wasm32-unknown-unknown`**,
> because kevy's clock reads `std::time::Instant::now()`, which that target has
> no implementation for. The compile-only CI check never caught this; *running*
> the module does. Making TTL/expiry work on wasm needs an `Instant`→ns clock
> port with a host-fed time source (on the roadmap). **Until then, treat wasm
> kevy-embedded as a non-expiring in-memory map** — `Config::default()`, no TTL
> ops, no `DEL`. The non-expiring core is fully functional.

Three WASM runtimes are explicitly supported:

| Runtime | Target triple | Threads | Persistence | Use case |
|---|---|---|---|---|
| Browser | `wasm32-unknown-unknown` | no | in-memory only | client-side cache, JS interop |
| WASI | `wasm32-wasip1` | no | yes (preopened dirs) | wasmtime, wasmer, server-side WASI hosts |
| Cloudflare Workers | `wasm32-unknown-unknown` (with Workers shim) | no | KV-binding bridge (out of scope here) | edge cache |

## Compile checks

```bash
# Browser-style WASM (no JS bindings here; user wires their own)
cargo check --target wasm32-unknown-unknown -p kevy-embedded

# WASI (file-system persistence via std::fs over preopened directories)
cargo check --target wasm32-wasip1 -p kevy-embedded
```

Both succeed today against the v1.0 codebase.

## Required configuration

### TTL reaper must be `Manual` on browser-style wasm32

`wasm32-unknown-unknown` has no thread-spawning runtime, so the default
`TtlReaperMode::Background` (which calls `std::thread::Builder::spawn`) fails —
open with the manual reaper:

```rust
use kevy_embedded::{Config, Store};

let s = Store::open(Config::default().with_ttl_reaper_manual())?;
```

> ⚠️ **Not yet usable on `wasm32-unknown-unknown`.** `Store::tick()` — and any
> TTL op — currently traps, because the reaper reads `Instant::now()`. The
> manual-reaper + host-driven `tick()` design above is the *intended* shape
> **once the clock port lands** (host feeds time → `tick(now_ms)`), but today it
> panics. Use the non-expiring core only. (WASI `wasm32-wasip1` has a working
> `Instant`, so this gap is specific to the browser/`-unknown-unknown` target.)

### WASI persistence needs preopened directories

`std::fs::File::create` and friends work on `wasm32-wasip1` ONLY when the
host has granted the WASM module access to a directory via `--dir` (or the
equivalent runtime API). Plumb the persisted path through `Config::with_persist`
and ensure the runtime invocation grants it:

```bash
wasmtime --dir=/data myapp.wasm
```

Inside Rust:

```rust
let s = Store::open(
    Config::default()
        .with_persist("/data")
        .with_ttl_reaper_manual()
)?;
```

WASI shells like wasmtime and wasmer will route the `/data` reads/writes
through to the host directory you mapped.

### Cloudflare Workers

Workers run WASM in a `wasm32-unknown-unknown`-style sandbox without
direct file access. Use kevy-embedded's pure-in-memory mode and route
durability through the platform's KV bindings on the JS side. The
`Store::log(...)` escape hatch lets you mirror any write to a custom
sink — implement an external "AOF" via Workers KV writes from JS, then
let kevy-embedded handle the in-memory state.

## What does NOT work on WASM

| Feature | Reason | Workaround |
|---|---|---|
| `kevy::serve()` (TCP server) | wasm32 has no sockets | use kevy-embedded in-process |
| **All TTL/expiry + `DEL` on `wasm32-unknown-unknown`** | the clock reads `Instant::now()`, unimplemented on that target → traps | none yet — non-expiring core only. **Tracked: `Instant`→ns clock port.** Works on `wasm32-wasip1` (WASI has a real `Instant`). |
| `TtlReaperMode::Background` on `wasm32-unknown-unknown` | no thread runtime | use `with_ttl_reaper_manual()` (open succeeds; TTL itself still gated by the row above) |
| AOF on browser wasm32 | no file system | pure in-memory `Config::default()` |
| BGREWRITEAOF on browser wasm32 | no AOF | n/a |
| Atomic `rename(2)` semantics on KV-backed Workers | KV is eventually consistent | snapshot serialisation handled at the JS layer |

## Dependency note

`kevy-embedded` itself ships zero crates.io dependencies. The browser /
Cloudflare integrations need `wasm-bindgen` (browser DOM interop) or
`worker` (Cloudflare) — those are app-level dependencies, NOT
kevy-embedded's, and you wire them yourself in your downstream crate.
We deliberately do not ship a `examples/wasm-browser` here so the
in-tree crates stay zero-dependency; instead, users build their own browser
bridge against the public `kevy_embedded::Store` API.
