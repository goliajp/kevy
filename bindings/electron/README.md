# @goliapkg/kevy-electron

kevy embedded in an Electron app — the **real native engine**, not a
re-implementation. One kevy store lives in the **main process** (loaded
through the Node door's stable Node-API addon); every renderer reaches it
through an async `window.kevy` exposed by a **contextBridge preload over
IPC**, safe under `contextIsolation: true` and `sandbox: true`.

```
┌──────────── main process ────────────┐        ┌──── renderer(s) ────┐
│  @goliapkg/kevy-node  →  one Store    │        │  window.kevy.set()  │
│  installKevyMain({ ipcMain, dir })    │◀─IPC──▶│  window.kevy.get()  │
│  (native engine, persistence, pub/sub)│        │  window.kevy.subscribe()
└───────────────────────────────────────┘        └─────────────────────┘
        ▲ contextBridge preload (kevy-electron/preload) bridges the two
```

## Install

```
npm install @goliapkg/kevy-electron
```

It pulls in `@goliapkg/kevy-node`, whose prebuilt native addon is selected
per-platform (darwin-arm64, linux-x64, linux-arm64) via `optionalDependencies`.
`electron` is a peer dependency (`>=28`, for ESM in the main process).

## Quick start

**Main process** — open the store once and register the IPC handlers:

```js
import { app, BrowserWindow, ipcMain } from "electron";
import { createRequire } from "node:module";
import { installKevyMain } from "@goliapkg/kevy-electron";

const require = createRequire(import.meta.url);
let kevy;

app.whenReady().then(async () => {
  kevy = await installKevyMain({ ipcMain, dir: "userData/kevy" }); // omit dir = in-memory

  const win = new BrowserWindow({
    webPreferences: {
      preload: require.resolve("@goliapkg/kevy-electron/preload"),
      contextIsolation: true, // default since Electron 12
      sandbox: true,          // default since Electron 20
    },
  });
  win.loadFile("renderer.html");
});

app.on("before-quit", () => kevy?.dispose());
```

**Renderer** — use `window.kevy` (async; the engine never enters the page):

```js
await window.kevy.set("greeting", "hello");
await window.kevy.getText("greeting");            // "hello"

await window.kevy.cmd("HSET", "u:1", "name", "Ada"); // every verb via cmd()

const stop = await window.kevy.subscribe("room", (payload, channel) => {
  console.log(channel, new TextDecoder().decode(payload));
});
await window.kevy.publish("room", "ping");        // → the callback above
// later: await stop();
```

See [`example/`](./example) for a runnable app (SET/GET + a live pub/sub demo).

## The API on `window.kevy`

Async, `contextIsolation`-safe. Typed verbs **throw** on a protocol error
(the TS client idiom); `cmd()` is the neutral escape hatch that returns a
protocol error as a `KevyError` **value** and reaches all ~184 verbs.

| Method | Returns |
|---|---|
| `cmd(...argv)` | `Promise<Reply>` — every verb; error as a `KevyError` value |
| `get(key)` / `getText(key)` | `Promise<Uint8Array \| null>` / `Promise<string \| undefined>` |
| `set(key, value, { ttlMs? })` | `Promise<void>` |
| `del(...keys)` | `Promise<number>` |
| `incrby(key, delta?)` | `Promise<number>` |
| `expire(key, ttlMs)` / `ttl(key)` | `Promise<boolean>` / `Promise<number>` (PTTL) |
| `mget(...keys)` | `Promise<(Uint8Array \| null)[]>` |
| `publish(channel, payload)` | `Promise<number>` |
| `subscribe(channel, cb)` / `psubscribe(pattern, cb)` | `Promise<Unsubscribe>` — streamed over IPC |
| `version()` | `Promise<string>` |

Keys and values are binary-safe: pass a `string` (UTF-8 encoded) or a
`Uint8Array` (passed through). The command family mirrors the
[client contract](../../docs/client-contract.md) §3.1/§3.11.

## The ABI-stability story — no electron-rebuild

**The same native addon that runs under Node runs unchanged inside Electron
— no `electron-rebuild`, no per-Electron-version recompile.**

Electron's general rule is that native modules must be rebuilt for it (its ABI
differs from stock Node — Chromium's BoringSSL vs OpenSSL, its own V8). The
**documented exception is Node-API (N-API)**: modules built against the stable
Node-API C ABI are forward-compatible across Node *and* Electron versions
without recompilation. kevy's Node door is exactly that:

- `crates/kevy-napi` is a **hand-written Node-API addon** (no `napi` crate). It
  declares only **Node-API version 1** symbols — the ABI stable since Node 8 —
  and exports `napi_register_module_v1`, which Electron's bundled Node calls on
  `process.dlopen` the same way stock Node does.
- It sidesteps the one class of N-API modules that *did* break on Electron 21+
  (the V8 memory-cage / pointer-compression sandbox). Replies come back through
  `napi_create_buffer_copy` — **V8 allocates the backing store** inside the
  cage and the engine copies into it — never `napi_create_external_buffer`
  (which points outside the cage and is what broke). Store/subscription handles
  cross as `napi_create_external` **External values** (opaque, not ArrayBuffers,
  so the cage rule doesn't apply). Nothing here depends on Electron internals.

**Evidence in this repo:** the addon builds with `cargo build -p kevy-napi`,
and the Node door's own suite (`bindings/node/node.test.js`) passes on the
Node this workspace ships — the identical `.node`/cdylib is what Electron loads.

> If you fork the addon to return external buffers, that story no longer holds
> and you would need `electron-rebuild`. As written, you do not.

## contextIsolation- and sandbox-safe by construction

- **The engine never enters the renderer.** Only the main process links
  kevy-node; the renderer holds a handful of async functions.
- **The preload is one self-contained CommonJS file.** A sandboxed renderer's
  polyfilled `require()` can only reach `electron` and a few Node built-ins —
  not a sibling module — so `preload.cjs` inlines everything it needs and
  requires no bundler. Point `webPreferences.preload` straight at it via
  `require.resolve("@goliapkg/kevy-electron/preload")`.
- **`contextBridge` is used only when isolation is on.** With
  `contextIsolation: true` the API is exposed with
  `contextBridge.exposeInMainWorld("kevy", …)`; if you have turned isolation
  off, the preload falls back to attaching `window.kevy` on the page global
  (per Electron's security guidance). Isolation on is strongly recommended.
- **No `nodeIntegration`, no remote module, no `eval`.** All traffic is typed
  IPC over named channels (`kevy:*`).

## Main-process store vs wasm-in-renderer — which to choose

kevy ships a second Electron-capable door: the **wasm build**
([`@goliapkg/kevy`](../../crates/kevy-wasm), the wasm package) runs the engine
*fully inside a renderer*, in that renderer's memory, with no IPC. Pick by
whether renderers must **share state**:

| | **Main-process store** (this package) | **wasm in the renderer** (`@goliapkg/kevy`) |
|---|---|---|
| Where the engine runs | main process | inside each renderer |
| State sharing | **one store shared by all renderers** | **isolated per renderer** — each has its own |
| Persistence | native AOF + snapshot to a `dir` on disk | OPFS / IndexedDB (per renderer/origin) |
| Cross-renderer pub/sub | yes — one bus in the main process | no (each renderer's bus is its own; wasm uses BroadcastChannel across *tabs*, not Electron renderers) |
| IPC on the hot path | yes (async invoke per op) | none — synchronous in-process calls |
| Native addon | yes (per-platform prebuilt) | no — one portable `.wasm`, any arch |

**Rule of thumb:**

- **Use the main-process store** (this package) when renderers must see the
  **same data** — a shared cache/session store, a durable app database on
  disk, pub/sub between windows. This is the primary, recommended design.
- **Use wasm in the renderer** when each window wants its **own** ephemeral
  in-memory store with no shared state and no IPC latency (a scratch cache, an
  offline sandbox), or when you want to avoid shipping a native addon entirely.

The two can coexist: a shared main-process store for durable/shared data plus a
per-renderer wasm store for local scratch.

## Testing

The bridge and preload are dependency-injected (`ipcMain` / `ipcRenderer` are
passed in), so their whole surface is unit-tested **headlessly** — a real
in-memory engine behind a fake `ipcMain`, and the renderer API against a fake
`ipcRenderer`. No display, no `xvfb`, no window:

```
cargo build -p kevy-napi        # from the repo root: the Node-door addon
cd bindings/electron && node --test test/*.test.js
```

Launching the actual `example/` window is the part that needs a desktop
session; the logic it exercises is what the headless tests already cover.

## License

Apache-2.0 OR MIT.
