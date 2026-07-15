# kevy in Electron

kevy embeds in an Electron app two ways. The package
[`@goliapkg/kevy-electron`](../bindings/electron) is the **primary** door: the
real native engine runs once in the **main process** and every renderer reaches
it over a `contextBridge` preload + IPC. The **wasm** build
([`docs/wasm.md`](wasm.md)) is the alternative: the engine runs *inside* a
renderer with no IPC and no shared state.

## Primary: one store in the main process

```
┌──────────── main process ────────────┐        ┌──── renderer(s) ────┐
│  @goliapkg/kevy-node → one Store      │◀─IPC──▶│  window.kevy.*      │
└───────────────────────────────────────┘        └─────────────────────┘
```

The main process links the Node door (`@goliapkg/kevy-node`, a stable
Node-API addon over `kevy-ffi`); the renderer only ever holds the async
`window.kevy` the preload exposes. Safe under `contextIsolation: true` and
`sandbox: true` (Electron's secure defaults).

```js
// main process
import { ipcMain, BrowserWindow } from "electron";
import { createRequire } from "node:module";
import { installKevyMain } from "@goliapkg/kevy-electron";
const require = createRequire(import.meta.url);

const kevy = await installKevyMain({ ipcMain, dir: "userData/kevy" });
new BrowserWindow({
  webPreferences: {
    preload: require.resolve("@goliapkg/kevy-electron/preload"),
    contextIsolation: true,
    sandbox: true,
  },
});
```

```js
// renderer
await window.kevy.set("k", "v");
await window.kevy.getText("k");                 // "v"
await window.kevy.cmd("HSET", "u:1", "n", "Ada"); // every verb
const stop = await window.kevy.subscribe("room", (p, ch) =>
  console.log(ch, new TextDecoder().decode(p)));
await window.kevy.publish("room", "hi");        // streams to the callback
```

The verb surface mirrors the [client contract](client-contract.md) §3.1
(core KV) and §3.11 (pub/sub). Full method table and a runnable example:
[`bindings/electron/README.md`](../bindings/electron/README.md).

## No electron-rebuild — the Node-API story

The same native addon runs under Node **and** Electron with no recompile.
Electron normally requires rebuilding native modules for its ABI; the
documented exception is **Node-API (N-API)**, and kevy's addon
(`crates/kevy-napi`) is a hand-written Node-API **version 1** module. It also
avoids the Electron-21+ V8 memory-cage pitfall: replies come back through
`napi_create_buffer_copy` (V8 allocates the buffer inside the cage), never an
external buffer, and handles cross as opaque External values. Full argument
and evidence: the binding's README, section "The ABI-stability story".

## Alternative: wasm in the renderer

`@goliapkg/kevy` runs the engine fully inside a renderer — synchronous,
no IPC, no native addon, one portable `.wasm`. The trade is **isolation**:
each renderer gets its **own** store, and there is no shared main-process bus.

| Choose | When |
|---|---|
| **Main-process store** (`@goliapkg/kevy-electron`) | renderers must share data / a durable on-disk DB / cross-window pub/sub |
| **wasm in renderer** (`@goliapkg/kevy`) | each window wants its own ephemeral store, no IPC latency, no native addon |

They coexist: a shared main-process store for durable/shared state plus a
per-renderer wasm store for local scratch. See the binding README's
"Main-process store vs wasm-in-renderer" table for the full comparison.
