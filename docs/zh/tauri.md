# 在 Tauri 应用里用 kevy

[Tauri](https://tauri.app) 应用的后端就是 Rust——所以 kevy 直接嵌入，没有服务器、没有独立进程。[`tauri-plugin-kevy`](https://github.com/goliajp/kevy/tree/develop/bindings/tauri) 在 Tauri 的托管状态里持有一份 [`kevy-embedded`](../embedded-listener.md) `Store`，并经 `invoke` 暴露给 webview。一份共享的 store，每个窗口和你的 Rust 代码都能同样触达。

完整指南、安装步骤与 API 表：**[bindings/tauri/README.md](https://github.com/goliajp/kevy/tree/develop/bindings/tauri)**。

## 形状

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

webview 触达整个引擎：带类型的 `get`/`set`/`del`/`incr`，经 Tauri `Channel` 流式送达的 pub/sub，以及一条通向其余所有动词的裸 `cmd(argv)` 路径（`HSET`、`ZADD`、`IDX.*`……）——与各语言客户端实现的是同一份[客户端契约](../client-contract.md)。

## 两扇门：后端 store vs. wasm

kevy 也可以[整个跑在 webview 里（WASM）](wasm.md)。按数据住在哪来选：

- **后端 store**（本插件）——一份原生 store，跨窗口、跨 Rust 共享，持久（快照 + AOF），每次调用一跳 IPC。应用的真实数据放这里。
- **webview 里的 wasm**——隔离、临时、只属于单个 webview 的 store，零 IPC。沙箱草稿区。

完整决策表见[绑定 README](https://github.com/goliajp/kevy/tree/develop/bindings/tauri#shared-store-this-plugin-vs-wasm-in-webview)。

## 零依赖边界

kevy 的工作区严格零依赖。而 Tauri 插件必须依赖 `tauri` crate，所以 `tauri-plugin-kevy` 位于工作区**之外**（根 `Cargo.toml` 的 `exclude`），依赖 `tauri` + 零依赖的 `kevy-embedded` 与 `kevy-resp`。仓库根的 `cargo build` 永远看不到 `tauri`；引擎保持纯净。
