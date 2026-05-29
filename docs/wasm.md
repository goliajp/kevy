# kevy on WebAssembly

`kevy-embedded` (the in-process variant of kevy — see
[`crates/kevy-embedded/README.md`](../crates/kevy-embedded/README.md)) targets
WebAssembly out of the box. The full `kevy` server (`kevy-rt`, `kevy-sys`)
does **not** — it needs sockets, threads, and OS pollers that WASM runtimes
either don't expose or expose only as stubs.

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

`wasm32-unknown-unknown` has no thread-spawning runtime. The default
`TtlReaperMode::Background` calls `std::thread::Builder::spawn` which
returns `Err` on that target — `Store::open` then propagates the error.
**Always** use:

```rust
use kevy_embedded::{Config, Store};

let s = Store::open(
    Config::default().with_ttl_reaper_manual()
)?;
```

…and call `Store::tick()` from whichever event source you control (animation
frame, polling timer, postMessage handler). 10× per second matches Redis's
`hz=10`; under-ticking just means TTL'd keys linger slightly longer before
the active reaper picks them up (lazy expiry on access still works).

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
| `TtlReaperMode::Background` on `wasm32-unknown-unknown` | no thread runtime | use `with_ttl_reaper_manual()` + `Store::tick()` |
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
