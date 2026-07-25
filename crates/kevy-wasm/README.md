# kevy-wasm

WebAssembly bindings for [kevy](https://github.com/goliajp/kevy) — the embedded Redis-compatible KV engine behind a hand-written C ABI for browsers and JS runtimes. No binding generator, no JS-side dependencies: a bare `wasm32-unknown-unknown` module plus a small hand-written ES-module loader.

- **KV + TTL** — `set` / `get` / `del` / `expire` / counters / scans, same engine as the kevy server.
- **Pub/sub** — in-instance subscriptions with a polling drain; the JS loader bridges tabs over `BroadcastChannel`.
- **Host-mediated persistence** — writes emit standard kevy AOF frames; the loader pumps them into OPFS (or IndexedDB) and replays them on the next open. The log is byte-compatible with a native kevy AOF.

## Use from JavaScript

The published npm package `@goliapkg/kevy` wraps this module; see [`pkg/`](pkg/) for the loader, typings, and worker.

```js
import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });
await db.set("greeting", "hello");
db.getText("greeting"); // "hello"
```

## Build

```sh
cargo build -p kevy-wasm --target wasm32-unknown-unknown --release
```

The artifact is `target/wasm32-unknown-unknown/release/kevy_wasm.wasm`; the loader in `pkg/kevy.js` instantiates it directly.

## ABI

See the crate docs for the full export list and conventions (handles, `(ptr, len)` byte passing, the per-instance result buffer, status codes, packed event/frame formats).

License: Apache-2.0 OR MIT, same as the kevy workspace.
