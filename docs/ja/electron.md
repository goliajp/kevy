# Electron で kevy を使う

kevy を Electron アプリに組み込む方法は二つあります。パッケージ [`@goliapkg/kevy-electron`](https://github.com/goliajp/kevy/tree/develop/bindings/electron) が**本命の扉**です：本物のネイティブエンジンが**メインプロセス**で一度だけ走り、各 renderer は `contextBridge` の preload + IPC を通じてそこへ届きます。**wasm** ビルド（[`docs/wasm.md`](wasm.md)）はもう一つの選択肢で、エンジンは renderer の*内側*で走ります——IPC も共有状態もありません。

## 本命：メインプロセスにひとつのストア

```
┌──────────── main process ────────────┐        ┌──── renderer(s) ────┐
│  @goliapkg/kevy-node → one Store      │◀─IPC──▶│  window.kevy.*      │
└───────────────────────────────────────┘        └─────────────────────┘
```

メインプロセスは Node の扉（`@goliapkg/kevy-node`、`kevy-ffi` 上の安定した Node-API アドオン）をリンクします。renderer が握るのは、preload が公開する非同期の `window.kevy` だけです。`contextIsolation: true` と `sandbox: true`（Electron の安全な既定値）の下で安全です。

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

動詞の面は[クライアント契約](../client-contract.md)の §3.1（コア KV）と §3.11（pub/sub）を鏡写しにします。メソッド表の全体と動く例：[`bindings/electron/README.md`](https://github.com/goliajp/kevy/blob/develop/bindings/electron/README.md)。

## electron-rebuild 不要——Node-API の話

同じネイティブアドオンが、再コンパイルなしで Node **と** Electron の両方で動きます。Electron は通常、ネイティブモジュールを自分の ABI 向けに再ビルドすることを要求します。文書化された例外が **Node-API（N-API）**であり、kevy のアドオン（`crates/kevy-napi`）は手書きの Node-API **version 1** モジュールです。Electron 21+ の V8 memory-cage の落とし穴も避けています：応答は `napi_create_buffer_copy` 経由で返り（V8 が cage の内側にバッファを確保します）、外部バッファは決して使わず、ハンドルは不透明な External 値として渡ります。完全な論証と証拠はバインディング README の「The ABI-stability story」節にあります。

## もう一つの扉：renderer の中の wasm

`@goliapkg/kevy` はエンジンを renderer の中で完結して走らせます——同期、IPC なし、ネイティブアドオンなし、可搬な `.wasm` ひとつ。引き換えは**隔離**です：各 renderer は**自分だけの**ストアを持ち、共有のメインプロセスバスはありません。

| 選ぶ | いつ |
|---|---|
| **メインプロセスのストア**（`@goliapkg/kevy-electron`） | renderer 間でデータを共有したい／ディスクに永続する DB が要る／ウィンドウをまたぐ pub/sub が要る |
| **renderer の wasm**（`@goliapkg/kevy`） | 各ウィンドウが自分の使い捨てストアを持ちたい、IPC レイテンシをゼロにしたい、ネイティブアドオンを避けたい |

両者は共存できます：永続・共有の状態は共有のメインプロセスストアに、ローカルな下書きは renderer ごとの wasm ストアに。完全な比較はバインディング README の「Main-process store vs wasm-in-renderer」表を参照してください。
