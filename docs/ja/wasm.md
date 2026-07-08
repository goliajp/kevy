# WebAssembly上のkevy

`kevy-embedded`とその依存クロージャはWebAssemblyにコンパイルできるため、同じプロセス内KVエンジンがブラウザ、エッジランタイム、WASIホストの中でそのまま動きます。

## このドキュメントが必要になるとき

- **ブラウザKV** — Webアプリ内の高速なインメモリkey/valueキャッシュを、サーバーで使うのと同じAPI面で使いたいとき。
- **Cloudflare Workers**（および類似のエッジランタイム） — プラットフォーム提供の永続ストアの手前に置く、isolate内ホットキャッシュ。
- **組み込みWASMキャッシュ** — 大きなホスト（ゲームエンジン、スクリプティングホスト、サーバーレスコンテナ）内のサンドボックス化プラグインで、ネットワークスタックを引き込まずにRedis型のストアを使いたいとき。
- **サーバー側WASIプラグイン** — `wasmtime`/`wasmer`配下の長寿命な`wasm32-wasip1`モジュールで、ホストファイルシステムへの永続化が必要なとき。

## 中心となる考え方

同じエンジンから2つのものを取り除いただけです。OSクロックとOSスレッドです。`kevy-embedded`は`kevy-store`、`kevy-persist`、`kevy-hash`、`kevy-bytes`、`kevy-map`、`kevy-resp`を引き込み、これらはすべて`wasm32-unknown-unknown`と`wasm32-wasip1`でビルドできます。ネットワークreactor系クレート（`kevy-rt`、`kevy-sys`、`kevy-uring`）は意図的にこのクロージャに含めていないため、WASMビルドはクリーンに通ります。通常ならTTLリーパースレッドをspawnするところで、代わりにホストのイベントループから呼ぶ`Store::tick()`を公開し、スレッドのないブラウザターゲットではホストが供給するクロックを読みます。データ構造、コマンド、永続化フォーマットは一切変わりません。

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

ホスト側の糊は小さくて済みます。ブラウザならJSの`setInterval(() => { mod.tick(now()); }, 100)`、WASI配下なら普通の`std::thread::sleep`ループです。それ以外の部分（`set`、`get`、`del`、ハッシュ、リスト、ソート済みセット、スクリプティング、AOF）は、Linuxで出荷するのと同じコードパスです。

## ビルドマトリクス

| ターゲット | Cargoコマンド | 注意 |
|---|---|---|
| `wasm32-unknown-unknown`（ブラウザ） | `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | スレッドなし。`Instant`/`SystemTime`もなし。ホストが[`set_clock_ns`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs)と[`set_wall_clock_ms`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-store/src/lib.rs)でクロックを供給する。永続化はインメモリディレクトリ。 |
| `wasm32-unknown-unknown`（Cloudflare Workers） | `cargo build --target wasm32-unknown-unknown -p kevy-embedded` | 同じモジュール。クロックソースにはWorkersランタイムの`Date.now()`を使う。耐久性のある永続化はJS側のWorkers KVバインディングを通す。 |
| `wasm32-wasip1`（サーバー側WASI） | `cargo build --target wasm32-wasip1 -p kevy-embedded` | スレッドはやはりないが、`Instant`と`SystemTime`が動くのでホストからクロックを供給する必要はない。`std::fs`はpreopenディレクトリ（`wasmtime --dir=/data`）に対して動く。 |
| ネイティブ（`x86_64-*`、`aarch64-*`） | `cargo build -p kevy-embedded` | 参考：デフォルトでバックグラウンドリーパースレッドをspawnするため、手動で駆動するものはない。 |

依存クロージャは[`crates/kevy-embedded/Cargo.toml`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/Cargo.toml)を、再エクスポートは[`crates/kevy-embedded/src/lib.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/src/lib.rs)を参照してください。

## ネイティブとの違い

| 関心事 | ネイティブ | WASM |
|---|---|---|
| TTLリーパー | バックグラウンドスレッドを自動spawn | 手動：`Config::with_ttl_reaper_manual()` + ホストが`Store::tick()`を呼ぶ |
| クロック | OSの`Instant`/`SystemTime` | `wasm32-wasip1`:OSから。`wasm32-unknown-unknown`:ホストが`set_clock_ns`/`set_wall_clock_ms`で供給 |
| ネットワークサーバー | `kevy-rt` + `kevy-sys` + `kevy-uring`がTCPでlisten | これらのクレートはWASMビルドクロージャに含まれない。`Store`で直接組み込む |
| 永続化 | `with_persist`に渡したディレクトリへAOF | `wasm32-wasip1`:同じ（preopenしたホストディレクトリに対して）。`wasm32-unknown-unknown`:インメモリディレクトリのみ（耐久性が欲しければホスト側へ書き込みをミラー） |
| 非同期ランタイム | ユーザーコードのTokio / stdスレッド | ホストが与えるもの（JSイベントループ、Workersのfetchハンドラ、WASIのシングルスレッドループ） |

## トレードオフ

- **TTLの精度はループ周期に追従します。** 500msのTTLを持つキーは、デッドライン後の最初の`tick()`ではじめてexpireします。100msのループが典型で、それより短くしても構いませんし、キャッシュ用途なら長めでも問題ありません。ただしエンジンは、ホストが与える周期以上の精度は出せません。
- **非同期ランタイムは同梱しません。** kevy-embeddedは`tokio`も`wasm-bindgen-futures`も引き込みません。ループはホストが所有し、ライブラリはマイクロ秒で終わる同期メソッドを公開します。
- **バックグラウンド作業がないので、不意打ちも隠れたコストもありません。** ただし`tick()`を呼び忘れると期限切れキーが生き続け、メモリが膨らみます。ほかの定期作業を仕込んでいるのと同じ場所に呼び出しを組み込んでください。
- **`wasm32-unknown-unknown`の耐久性は自動では得られません。** ファイルシステムがない以上、純粋なインメモリキャッシュとして走らせるか、ホスト側のシンク（Workers KV、IndexedDBなど）へ書き込みをミラーするかのどちらかです。

## FAQ

**ブラウザで動きますか？** はい。`wasm32-unknown-unknown`向けにビルドし、生成された`.wasm`を`wasm-bindgen`などのバインディングとともに出荷し、`Config::default().with_ttl_reaper_manual()`で開き、各`tick()`の前に`Date.now()`からクロックを供給します。コマンド面は完全に（文字列、ハッシュ、リスト、セット、ソート済みセット、pub/sub、スクリプティングまで）プロセス内で動きます。

**Cloudflare Workersでの最小セットアップは？** `kevy-embedded`を`wasm32-unknown-unknown`向けにコンパイルし、isolateごとに`Store`を1つインスタンス化し、`tick()`を遅延実行（TTLに敏感なreadの直前）またはスケジュールハンドラから呼びます。クロックソースはWorkersランタイムの`Date.now()`です。isolateの再起動をまたぐ耐久性が必要なら、JSハンドラからWorkers KVかD1へ書き込みをミラーしてください。エンジン自身はインメモリのままです。

**どうやって永続化しますか？** `wasm32-wasip1`では`Config::with_persist("/data")`を呼び、`wasmtime --dir=/data`（または使っているランタイムの相当機能）でモジュールを起動します。AOFはpreopenディレクトリに書かれ、次回のopenでリプレイされます。`wasm32-unknown-unknown`にはファイルシステムがないため、永続化はホスト介在にせざるを得ません。典型的には、プラットフォームが提供する耐久ストアへ書き込みをミラーします。

**スレッドは？ Atomics有効のWASMは？** デフォルトのWASMビルドはシングルスレッドで、現在出荷されているすべてのブラウザ系ターゲットと一致します。ホストランタイムが共有メモリスレッド（`wasm32-unknown-unknown`に`--target-feature=+atomics,+bulk-memory`とスレッドプール）を公開している場合でも`Store`は安全に使えますが、バックグラウンドリーパーモードはオフのままです。サポートされるのは手動`tick()`モデルで、あなたのコードのスレッドは`Store`を共有して並行に呼び出せます。
