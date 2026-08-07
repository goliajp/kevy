# Tauri アプリで kevy を使う

[Tauri](https://tauri.app) アプリのバックエンドは Rust です——だから kevy は直接組み込めます。サーバも別プロセスも要りません。[`tauri-plugin-kevy`](https://github.com/goliajp/kevy/tree/develop/bindings/tauri) は Tauri の管理ステートに [`kevy-embedded`](../embedded-listener.md) の `Store` をひとつ持ち、`invoke` 経由で webview に公開します。共有ストアがひとつ、どのウィンドウからも、あなたの Rust コードからも同じように届きます。

完全なガイド、インストール手順、API 表：**[bindings/tauri/README.md](https://github.com/goliajp/kevy/tree/develop/bindings/tauri)**。

## 形

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

webview からエンジン全体に届きます：型付きの `get`/`set`/`del`/`incr`、Tauri の `Channel` でストリームされる pub/sub、そして残りすべての動詞（`HSET`、`ZADD`、`IDX.*`……）への素の `cmd(argv)` 経路——各言語クライアントが実装しているのと同じ[クライアント契約](../client-contract.md)です。

## 二つの扉：バックエンドのストア vs. wasm

kevy は [webview の中で WASM として丸ごと](wasm.md)走らせることもできます。データがどこに住むかで選んでください：

- **バックエンドのストア**（このプラグイン）——ウィンドウと Rust をまたいで共有されるネイティブなストアひとつ、永続（スナップショット + AOF）、呼び出しごとに IPC 一跳。アプリの本当のデータはこちら。
- **webview の wasm**——単一の webview に閉じた、隔離された使い捨てストア、IPC なし。サンドボックスの下書き場。

完全な決定表は[バインディング README](https://github.com/goliajp/kevy/tree/develop/bindings/tauri#shared-store-this-plugin-vs-wasm-in-webview) にあります。

## 0 依存の境界

kevy のワークスペースは厳格に 0 依存です。しかし Tauri プラグインは `tauri` クレートに依存せざるを得ないので、`tauri-plugin-kevy` はワークスペースの**外**（ルート `Cargo.toml` の `exclude`）に置かれ、`tauri` + 0 依存の `kevy-embedded` と `kevy-resp` に依存します。リポジトリルートの `cargo build` が `tauri` を見ることは決してなく、エンジンは純粋なままです。
