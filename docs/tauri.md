# kevy in a Tauri app

A [Tauri](https://tauri.app) app's backend is Rust — so kevy embeds directly,
no server and no separate process. [`tauri-plugin-kevy`](https://github.com/goliajp/kevy/tree/develop/bindings/tauri)
holds one [`kevy-embedded`](./embedded-listener.md) `Store` in Tauri's managed
state and exposes it to the webview over `invoke`. One shared store, reachable
from every window and from your Rust code alike.

Full guide, install steps, and the API table: **[bindings/tauri/README.md](https://github.com/goliajp/kevy/tree/develop/bindings/tauri)**.

## The shape

```rust
// src-tauri/src/lib.rs
tauri::Builder::default()
    .plugin(tauri_plugin_kevy::init())              // in-memory
    // .plugin(tauri_plugin_kevy::Builder::new().path("./data").build())  // persistent
    .run(tauri::generate_context!())
    .unwrap();
```

```ts
// webview
import { kevy } from 'tauri-plugin-kevy-api'
await kevy.set('k', 'v')
await kevy.getString('k')                           // "v"
const sub = await kevy.subscribe({ channels: ['room'] }, (m) => { /* … */ })
await kevy.publish('room', 'hi')
```

The webview reaches the whole engine: typed `get`/`set`/`del`/`incr`, pub/sub
streamed over a Tauri `Channel`, and a raw `cmd(argv)` path for every other verb
(`HSET`, `ZADD`, `IDX.*`, …) — the same [client contract](./client-contract.md)
the language clients implement.

## Two doors: backend store vs. wasm

kevy can also run [entirely in the webview as WASM](./wasm.md). Choose by where
the data lives:

- **Backend store** (this plugin) — one native store shared across windows and
  Rust, durable (snapshot + AOF), one IPC hop per call. The app's real data.
- **wasm in the webview** — an isolated, ephemeral store scoped to a single
  webview, no IPC. Sandboxed scratch space.

The [binding README](https://github.com/goliajp/kevy/tree/develop/bindings/tauri#shared-store-this-plugin-vs-wasm-in-webview)
has the full decision table.

## The 0-dep boundary

kevy's workspace is strictly 0-dependency. A Tauri plugin must depend on the
`tauri` crate, so `tauri-plugin-kevy` sits **outside** the workspace (root
`Cargo.toml` `exclude`), depending on `tauri` + the 0-dep `kevy-embedded` and
`kevy-resp`. `cargo build` at the repo root never sees `tauri`; the engine stays
pure.
