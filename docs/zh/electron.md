# 在 Electron 里用 kevy

kevy 有两种方式嵌进 Electron 应用。包 [`@goliapkg/kevy-electron`](https://github.com/goliajp/kevy/tree/develop/bindings/electron) 是**主门**：真正的原生引擎在**主进程**里只跑一份，每个 renderer 经由 `contextBridge` preload + IPC 触达它。**wasm** 构建（[`docs/wasm.md`](wasm.md)）是备选：引擎跑在 renderer *内部*，没有 IPC、也没有共享状态。

## 主门：主进程里的一份 store

```
┌──────────── main process ────────────┐        ┌──── renderer(s) ────┐
│  @goliapkg/kevy-node → one Store      │◀─IPC──▶│  window.kevy.*      │
└───────────────────────────────────────┘        └─────────────────────┘
```

主进程链接 Node 门（`@goliapkg/kevy-node`，一个基于 `kevy-ffi` 的稳定 Node-API addon）；renderer 手里始终只有 preload 暴露出来的异步 `window.kevy`。在 `contextIsolation: true` 与 `sandbox: true`（Electron 的安全默认值）下安全。

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

动词面镜像[客户端契约](../client-contract.md) §3.1（核心 KV）与 §3.11（pub/sub）。完整方法表与可跑示例：[`bindings/electron/README.md`](https://github.com/goliajp/kevy/blob/develop/bindings/electron/README.md)。

## 不需要 electron-rebuild —— Node-API 的故事

同一个原生 addon 在 Node **和** Electron 下运行，无需重编译。Electron 通常要求为它的 ABI 重建原生模块；文档写明的例外是 **Node-API（N-API）**，而 kevy 的 addon（`crates/kevy-napi`）是手写的 Node-API **version 1** 模块。它同时避开了 Electron 21+ 的 V8 memory-cage 陷阱：回复经由 `napi_create_buffer_copy` 返回（V8 在 cage 内分配缓冲区），从不使用外部缓冲区，句柄以不透明的 External 值穿越。完整论证与证据：绑定 README 的 "The ABI-stability story" 一节。

## 备选：renderer 里的 wasm

`@goliapkg/kevy` 让引擎完整跑在一个 renderer 内——同步、无 IPC、无原生 addon、一个可移植的 `.wasm`。代价是**隔离**：每个 renderer 拿到的是**自己的** store，没有共享的主进程总线。

| 选 | 什么时候 |
|---|---|
| **主进程 store**（`@goliapkg/kevy-electron`） | renderer 之间要共享数据 / 要一个持久的磁盘 DB / 要跨窗口 pub/sub |
| **renderer 里的 wasm**（`@goliapkg/kevy`） | 每个窗口要自己的临时 store、零 IPC 延迟、不要原生 addon |

两者可以共存：一份共享的主进程 store 承担持久/共享状态，外加每个 renderer 一份 wasm store 做本地草稿。完整对比见绑定 README 的 "Main-process store vs wasm-in-renderer" 表。
