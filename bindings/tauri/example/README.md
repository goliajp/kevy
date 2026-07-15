# kevy + Tauri example

A minimal Tauri v2 app that embeds the kevy engine via
[`tauri-plugin-kevy`](../tauri-plugin-kevy) and drives it from a tiny static
frontend: string set/get, live pub/sub, and a raw-command console.

## Layout

```
example/
├── package.json          # @tauri-apps/cli for `tauri dev` / `tauri build`
├── src/                  # static frontend (no bundler — withGlobalTauri)
│   ├── index.html
│   └── main.js           # invoke('plugin:kevy|…') + a Channel for pub/sub
└── src-tauri/
    ├── Cargo.toml        # standalone; depends on tauri + tauri-plugin-kevy (path)
    ├── tauri.conf.json   # frontendDist ../src, window label "main"
    ├── build.rs          # tauri_build::build()
    ├── capabilities/
    │   └── default.json  # grants "kevy:default" to the main window
    └── src/
        ├── lib.rs        # .plugin(tauri_plugin_kevy::init())
        └── main.rs
```

## Run

Requires the [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/)
(a Rust toolchain and your platform's WebView: WKWebView on macOS,
WebView2 on Windows, WebKitGTK on Linux).

```bash
cd bindings/tauri/example
npm install
npm run dev        # tauri dev — opens the window
# or
npm run build      # tauri build — a distributable (bundle disabled by default)
```

The store here is **in-memory** (`init()`), so it resets each launch. Swap it
for `Builder::new().path(dir).build()` in `src-tauri/src/lib.rs` to make it
persistent (snapshot + AOF).

## What it demonstrates

- **set / get** — typed scalar commands, keys/values as binary-safe byte arrays.
- **pub/sub** — `subscribe` opens a Tauri `Channel`; the backend poller streams
  every frame (acks + messages) into it; `publish` from the same window is
  received live. Two windows would share the one backend bus.
- **raw command** — any verb (`INCR`, `HSET`, `ZADD`, …) via `plugin:kevy|cmd`,
  with the decoded RESP reply rendered.

## Verified

`cargo check` on `src-tauri` passes in CI-less local runs — it compiles the app
against the plugin and assembles the plugin's ACL manifest + capabilities. A
full `tauri dev`/`build` additionally needs the platform WebView + the Tauri
CLI and is the manual step to see the window.
