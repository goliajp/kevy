# kevy on WebAssembly

`kevy-embedded` (the in-process variant of kevy — see
[`crates/kevy-embedded/README.md`](../crates/kevy-embedded/README.md)) **compiles
and runs** on WebAssembly. The **full in-memory KV — including TTL/expiry —
works today** on `wasm32-unknown-unknown` (`set` / `get` / `del` /
`set_with_ttl` / `pttl` / the reaper `tick`, verified end-to-end in Node; see
[`examples/wasm-kv/`](../examples/wasm-kv)). The full `kevy` server (`kevy-rt`,
`kevy-sys`) does **not** target wasm — it needs sockets, threads, and OS pollers
that WASM runtimes don't expose.

> ℹ️ **Host-fed clock required on `wasm32-unknown-unknown`.** That target has no
> `Instant`/`SystemTime` (calling them traps `unreachable`), so kevy's clock is
> cfg-gated to a **host-fed source**: the embedding advances time via
> [`kevy_embedded::set_clock_ns`] (monotonic ns, e.g. `Date.now() * 1e6`) — and
> [`set_wall_clock_ms`] if you use `XADD` auto-IDs / `EXPIREAT`. Feed it before
> TTL-sensitive ops and once per `tick`, and all of TTL/expiry/`DEL` work. (On
> native targets and WASI `wasm32-wasip1` the OS clock is used directly — no
> feeding needed.) **An earlier version of kevy trapped on every TTL op and
> `DEL` here, before this clock port landed.**

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

### Feed the host clock before TTL ops and each tick

On `wasm32-unknown-unknown` advance kevy's clock from the host, then drive the
manual reaper. A typical JS-side loop (using the `wasm-bindgen` wrapper from
[`examples/wasm-kv/`](../examples/wasm-kv)):

```js
setInterval(() => { cache.set_clock(Date.now()); cache.tick(); }, 100);
```

…where the wrapper forwards to the wasm-only setters:

```rust
use kevy_embedded::{set_clock_ns, set_wall_clock_ms};

// ms = Date.now(); call before TTL-sensitive ops and once per tick.
set_clock_ns(ms.saturating_mul(1_000_000)); // monotonic deadline clock
set_wall_clock_ms(ms);                       // wall clock (XADD/EXPIREAT)
store.tick();                                // active reaper sweep
```

Until the host feeds a value the clock reads `0`, so keys look live and never
expire early — the safe direction. (WASI `wasm32-wasip1` has a working `Instant`
and `SystemTime`, so no feeding is needed there.)

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
| `TtlReaperMode::Background` on `wasm32-unknown-unknown` | no thread runtime | use `with_ttl_reaper_manual()` + drive `tick()` from the host event loop |
| Self-advancing clock on `wasm32-unknown-unknown` | no `Instant`/`SystemTime` (they trap) | host feeds time via `set_clock_ns` / `set_wall_clock_ms`; then TTL/expiry/`DEL` all work (WASI `wasm32-wasip1` needs no feeding) |
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
