# WebAssembly 上の kevy

`kevy-embedded`(kevy のプロセス内バリアント ——
[`crates/kevy-embedded/README.md`](../../crates/kevy-embedded/README.md)
参照)は WebAssembly で **コンパイル可能・実行可能** です。**完全な
インメモリ KV —— TTL/expiry を含む —— が現在**、
`wasm32-unknown-unknown` で動作します(`set` / `get` / `del` /
`set_with_ttl` / `pttl` / reaper `tick`、Node でエンドツーエンド検証
済み;[`examples/wasm-kv/`](../../examples/wasm-kv) 参照)。フルの
`kevy` サーバー(`kevy-rt`、`kevy-sys`)は wasm を **ターゲットにし
ません** —— ソケット、スレッド、WASM runtime が公開しない OS poller
が必要だからです。

> ℹ️ **`wasm32-unknown-unknown` ではホスト供給時計が必要**。このター
> ゲットは `Instant`/`SystemTime` を持たない(呼ぶと `unreachable` が
> trap)ため、kevy のクロックは **ホスト供給ソース** に cfg-gate され
> ています:埋め込み側が [`kevy_embedded::set_clock_ns`] で時間を進め
> (単調 ns、例えば `Date.now() * 1e6`)、`XADD` 自動 ID / `EXPIREAT`
> を使うなら [`set_wall_clock_ms`] も。TTL に敏感な操作の前、および
> 各 `tick` ごとに 1 回供給すれば、TTL/expiry/`DEL` すべて動作します。
> (ネイティブ・ターゲットと WASI `wasm32-wasip1` は OS の時計を直接
> 使うため、供給は不要です。)**clock port が入る前のバージョンの
> kevy はここで TTL 操作と `DEL` 毎に trap していました。**

明示的にサポートされる 3 つの WASM runtime:

| Runtime | Target triple | スレッド | 永続化 | 用途 |
|---------|--------------|--------|------|------|
| ブラウザ | `wasm32-unknown-unknown` | なし | インメモリのみ | クライアント・キャッシュ、JS 連携 |
| WASI | `wasm32-wasip1` | なし | あり(preopened dirs) | wasmtime、wasmer、サーバ側 WASI ホスト |
| Cloudflare Workers | `wasm32-unknown-unknown`(Workers shim 付き) | なし | KV-binding ブリッジ(本文の範囲外) | エッジ・キャッシュ |

## コンパイル・チェック

```bash
# ブラウザ風 WASM(JS バインディングは含まれない;ユーザが自分で配線)
cargo check --target wasm32-unknown-unknown -p kevy-embedded

# WASI(preopened directories 上の std::fs 経由のファイル・システム永続化)
cargo check --target wasm32-wasip1 -p kevy-embedded
```

どちらも v1.0 コードベースで現在通ります。

## 必要な設定

### ブラウザ風 wasm32 では TTL reaper を `Manual` に

`wasm32-unknown-unknown` にはスレッド生成 runtime がないため、デフォ
ルトの `TtlReaperMode::Background`(`std::thread::Builder::spawn` を
呼ぶ)は失敗します —— manual reaper で open します:

```rust
use kevy_embedded::{Config, Store};

let s = Store::open(Config::default().with_ttl_reaper_manual())?;
```

### TTL 操作前と各 tick でホスト時計を供給

`wasm32-unknown-unknown` 上では、ホストから kevy の時計を進めてから
manual reaper を駆動します。典型的な JS 側ループ
([`examples/wasm-kv/`](../../examples/wasm-kv) の `wasm-bindgen`
wrapper を使う場合):

```js
setInterval(() => { cache.set_clock(Date.now()); cache.tick(); }, 100);
```

…wrapper が wasm-only setter に転送:

```rust
use kevy_embedded::{set_clock_ns, set_wall_clock_ms};

// ms = Date.now(); TTL 敏感 op の前、および各 tick ごとに 1 回呼ぶ。
set_clock_ns(ms.saturating_mul(1_000_000)); // 単調 deadline 時計
set_wall_clock_ms(ms);                       // 壁時計(XADD/EXPIREAT)
store.tick();                                // 能動 reaper sweep
```

ホストが値を供給するまで時計は `0` を読み、key は生きているように見え、
早期に期限切れすることはない —— 安全方向です。(WASI `wasm32-wasip1`
は動作する `Instant` と `SystemTime` を持つので、そこでは供給不要です。)

### WASI 永続化には preopened ディレクトリが必要

`std::fs::File::create` 等は `wasm32-wasip1` 上で動くのは、ホストが
`--dir`(または同等の runtime API)経由で WASM モジュールにそのディ
レクトリへのアクセスを認可している場合のみです。永続化パスを
`Config::with_persist` 経由で渡し、runtime 起動でも認可することを確
認してください:

```bash
wasmtime --dir=/data myapp.wasm
```

Rust 内で:

```rust
let s = Store::open(
    Config::default()
        .with_persist("/data")
        .with_ttl_reaper_manual()
)?;
```

wasmtime や wasmer のような WASI shell は `/data` の読み書きをマップ
したホスト・ディレクトリにルーティングします。

### Cloudflare Workers

Workers は直接のファイル・アクセスのない `wasm32-unknown-unknown` 風
サンドボックスで WASM を実行します。kevy-embedded の純インメモリ・
モードを使い、永続性は JS 側のプラットフォーム KV bindings 経由で
ルーティングします。`Store::log(...)` のエスケープ・ハッチを使えば、
書き込みをカスタム sink にミラーできます —— JS からの Workers KV 書
き込みで外部 "AOF" を実装し、kevy-embedded には in-memory 状態を担当
させてください。

## WASM 上で **動作しない** もの

| 機能 | 理由 | 回避策 |
|------|------|--------|
| `kevy::serve()`(TCP サーバー) | wasm32 にソケットなし | kevy-embedded のプロセス内利用 |
| `wasm32-unknown-unknown` 上の `TtlReaperMode::Background` | スレッド runtime なし | `with_ttl_reaper_manual()` + ホスト・イベント・ループから `tick()` 駆動 |
| `wasm32-unknown-unknown` 上の自走時計 | `Instant`/`SystemTime` なし(trap) | ホストから `set_clock_ns` / `set_wall_clock_ms` で供給;TTL/expiry/`DEL` 全動作(WASI `wasm32-wasip1` は供給不要) |
| ブラウザ wasm32 上の AOF | ファイル・システムなし | 純インメモリ `Config::default()` |
| ブラウザ wasm32 上の BGREWRITEAOF | AOF なし | n/a |
| KV-backed Workers 上のアトミック `rename(2)` セマンティクス | KV は eventually consistent | snapshot シリアライゼーションは JS 層で処理 |

## 依存に関する注記

`kevy-embedded` 自体は crates.io 依存ゼロ。ブラウザ / Cloudflare 統合
には `wasm-bindgen`(ブラウザ DOM 連携)や `worker`(Cloudflare)が
必要 —— それらはアプリ・レベルの依存であり、**kevy-embedded** のもの
ではなく、ダウンストリーム crate で自分で配線します。我々は意図的に
`examples/wasm-browser` を ship していません ——in-tree crate をゼロ
依存に保つため。ユーザは公開 `kevy_embedded::Store` API に対して自分
のブラウザ・ブリッジを構築してください。
