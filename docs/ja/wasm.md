# WebAssembly 上の kevy

`kevy-embedded` とその依存閉包は WebAssembly にコンパイルされるので、同じプロセス内 KV エンジンがブラウザ、エッジランタイム、WASI ホストの中で動きます。

## このドキュメントが必要になるとき

- **ブラウザ KV** — Web アプリ内の高速インメモリ key/value キャッシュ。サーバーで使うのと同じ API 面で。
- **Cloudflare Workers**(および類似のエッジランタイム) — プラットフォーム提供の永続ストアの前に置く、isolate 内のホットキャッシュ。
- **組み込み WASM キャッシュ** — 大きなホスト(ゲームエンジン、スクリプティングホスト、サーバーレスコンテナ)内のサンドボックス済みプラグインで、ネットワークスタックを引かずに Redis 形状のストアを使いたい。
- **サーバー側 WASI プラグイン** — `wasmtime` / `wasmer` 下の長寿命 `wasm32-wasip1` モジュールで、ホストファイルシステムへの永続化が必要。

## 中心となる考え方

同じエンジンから 2 つのものを抜きます: OS クロックと OS スレッドです。`kevy-embedded` は `kevy-store`、`kevy-persist`、`kevy-hash`、`kevy-bytes`、`kevy-map`、`kevy-resp` を引き、これらすべては `wasm32-unknown-unknown` と `wasm32-wasip1` でビルドできます。ネットワーク reactor クレート(`kevy-rt`、`kevy-sys`、`kevy-uring`)は意図的にその閉包に含めないので、WASM ビルドはクリーンです。エンジンが通常 TTL リーパースレッドを spawn する代わりに、ホストのイベントループから呼ぶ `Store::tick()` を公開し、スレッドなしのブラウザターゲットではホストが供給するクロックを読みます。データ構造、コマンド、永続化フォーマットは変わりません。

## 動かしてみる例

```rust
use kevy_embedded::{Config, Store, set_clock_ns, set_wall_clock_ms};

// 1. スレッドを spawn しないよう、手動リーパーで開く。
let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// 2. エンジンを使う。wasm32-unknown-unknown ではまずクロックを供給。
//    wasm32-wasip1 とネイティブでは OS から読まれる。
set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
set_wall_clock_ms(now_ms_from_host());

store.set(b"hello", b"world")?;
let v = store.get(b"hello")?;            // Some(b"world".to_vec())
store.set_with_ttl(b"flash", b"x", std::time::Duration::from_millis(500))?;

// 3. ホストループからエビクションを駆動。Web なら setInterval /
//    requestAnimationFrame でスケジュール、WASI 下ならただの sleep ループ。
loop {
    set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
    set_wall_clock_ms(now_ms_from_host());
    let _stats = store.tick();           // 期限切れキーを expire
    host_sleep_ms(100);
}
```

ホスト側の糊は小さくて済みます: ブラウザ用に JS の `setInterval(() => { mod.tick(now()); }, 100)`、WASI 下なら通常の `std::thread::sleep` ループです。それ以外 — `set`、`get`、`del`、ハッシュ、リスト、ソート済みセット、スクリプティング、AOF — は Linux で出荷するのと同じコードパスです。

## ビルドマトリクス

| ターゲット | Cargo コマンド | 注意 |
|---|---|---|
| `wasm32-unknown-unknown`(ブラウザ) | `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | スレッドなし。`Instant` / `SystemTime` なし — ホストが [`set_clock_ns`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs) と [`set_wall_clock_ms`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs) でクロックを供給。永続化はインメモリディレクトリ。 |
| `wasm32-unknown-unknown`(Cloudflare Workers) | `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | 同じモジュール。クロックソースとして Workers ランタイムの `Date.now()` を使う。耐久性のある永続化は JS 側で Workers KV バインディングを通す。 |
| `wasm32-wasip1`(サーバー側 WASI) | `cargo build --target wasm32-wasip1 -p kevy-embedded` | スレッドはやはりなしだが、`Instant` と `SystemTime` が動くのでホストクロックの供給は不要。`std::fs` は preopen ディレクトリ(`wasmtime --dir=/data`)に対して動く。 |
| ネイティブ(`x86_64-*`、`aarch64-*`) | `cargo build -p kevy-embedded` | 参考: デフォルトでバックグラウンドリーパースレッドを spawn。手動で駆動する必要なし。 |

依存閉包は [`crates/kevy-embedded/Cargo.toml`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/Cargo.toml)、再 export は [`crates/kevy-embedded/src/lib.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/src/lib.rs) を参照。

## ネイティブとの違い

| 関心事 | ネイティブ | WASM |
|---|---|---|
| TTL リーパー | バックグラウンドスレッド、自動 spawn | 手動: `Config::with_ttl_reaper_manual()` + ホストが `Store::tick()` を呼ぶ |
| クロック | OS `Instant` / `SystemTime` | `wasm32-wasip1`: OS。`wasm32-unknown-unknown`: ホストが `set_clock_ns` / `set_wall_clock_ms` で供給 |
| ネットワークサーバー | `kevy-rt` + `kevy-sys` + `kevy-uring` が TCP で listen | これらのクレートは WASM ビルド閉包に含まれない。`Store` で直接組み込む |
| 永続化 | `with_persist` に渡したディレクトリへ AOF | `wasm32-wasip1`: 同じ、preopen ホスト dir に対して。`wasm32-unknown-unknown`: インメモリディレクトリのみ(耐久性が欲しければホスト側から書き出しをミラー) |
| 非同期ランタイム | ユーザーコードの Tokio / std スレッド | ホストが与えるもの(JS イベントループ、Workers fetch ハンドラ、WASI シングルスレッドループ) |

## トレードオフ

- **TTL 精度はループ周期に追従。** 500 ms TTL のキーはデッドライン後の次の `tick()` でだけ expire します。100 ms ループが典型で、それより詰めても大丈夫、キャッシュ用途なら緩くても大丈夫です。エンジンはホストが与えるよりよくはできません。
- **非同期ランタイムは同梱しません。** kevy-embedded は `tokio` も `wasm-bindgen-futures` も引きません。ループはホストが所有し、ライブラリはマイクロ秒で終わる同期メソッドを公開します。
- **バックグラウンド作業がないので意外なことも隠れたコストもありませんが**、`tick()` を忘れると expire 済みキーが生き続けてメモリが膨らみます。他の定期作業を仕込む場所と同じところに呼び出しを組み込んでください。
- **`wasm32-unknown-unknown` の耐久性は自動ではありません。** ファイルシステムなしでは純粋なインメモリキャッシュとして走るか、ホスト側シンク(Workers KV、IndexedDB 等)へ書き出しをミラーします。

## FAQ

**ブラウザで動きますか?** はい。`wasm32-unknown-unknown` 向けにビルドし、`wasm-bindgen` 等で結果の `.wasm` を出荷し、`Config::default().with_ttl_reaper_manual()` で開き、各 `tick()` の前に `Date.now()` からクロックを供給します。完全なコマンド面 — 文字列、ハッシュ、リスト、セット、ソート済みセット、pub/sub、スクリプティング — がプロセス内で動きます。

**Cloudflare Workers — 最小セットアップは?** `kevy-embedded` を `wasm32-unknown-unknown` 向けにコンパイルし、isolate ごとに `Store` を 1 つインスタンス化し、`tick()` を遅延(TTL 敏感な read の直前)またはスケジュールハンドラから呼びます。クロックソースは Workers ランタイムの `Date.now()`。isolate 再起動をまたぐ耐久性は、JS ハンドラから Workers KV または D1 に書き出しをミラーしてください。エンジン自身はインメモリのままです。

**どう永続化しますか?** `wasm32-wasip1` では `Config::with_persist("/data")` を呼び、`wasmtime --dir=/data`(またはランタイムの相当)でモジュールを起動します。AOF は preopen ディレクトリへ書かれ、次回 open でリプレイされます。`wasm32-unknown-unknown` ではファイルシステムがないので、永続化はホスト介在 — 典型的にはプラットフォーム提供の耐久ストアに書き出しをミラー — が必要です。

**スレッドは — Atomics 有効の WASM は?** デフォルトの WASM ビルドはシングルスレッドで、出荷中のすべてのブラウザ風ターゲットに一致します。ホストランタイムが共有メモリスレッド(`wasm32-unknown-unknown` の `--target-feature=+atomics,+bulk-memory` + スレッドプール)を公開しているなら、`Store` の使用は依然安全ですが、バックグラウンドリーパーモードは依然 off — 手動 `tick()` モデルがサポートされたパスで、あなたのコードのスレッドは `Store` を共有して並行で呼べます。
