# kevy Electron example

A minimal but real Electron app: kevy runs in the **main process** (the real
native engine, via `@goliapkg/kevy-electron`), and the renderer reaches it
through `window.kevy` — a contextBridge preload over IPC. `contextIsolation`
and `sandbox` are both on.

What it does:

- **SET / GET** a key — the value is stored by the engine in the main process
  and persisted under the app's `userData` dir.
- **Live pub/sub** — the page subscribes to a channel at startup; every
  PUBLISH round-trips through the engine and streams back over IPC into the log.

## Run

This example depends on the sibling package `@goliapkg/kevy-electron`, which in
turn uses the Node door (`@goliapkg/kevy-node`). In a published world:

```
npm install
npm start
```

In this repo (unpublished), link the workspace packages first (the native
addon must be built once — `cargo build -p kevy-napi` from the repo root), then
`npm install && npm start`. Launching the window needs a desktop session
(a display); the headless bridge/preload logic is covered by
`../test/*.test.js`, which run without one.

## Files

| File | Process | Role |
|---|---|---|
| `main.js` | main | opens one kevy store, `installKevyMain({ ipcMain, dir })`, creates the window |
| `renderer.html` + `renderer.js` | renderer | UI; talks only to `window.kevy` |
| preload | (renderer) | the package's `@goliapkg/kevy-electron/preload`, resolved in `main.js` |
