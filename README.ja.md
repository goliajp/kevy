# kevy

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg?branch=develop)](https://github.com/goliajp/kevy/actions/workflows/ci.yml?query=branch%3Adevelop)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Rust stable](https://img.shields.io/badge/rust-stable-orange.svg)

純粋なRustで書かれた、依存ゼロのRedis互換キーバリューストアです。
スタンドアロンサーバーとして、プロセス内ライブラリとして、あるいはその
両方として利用できます。どの形態でもRESP2を話すため、`redis-cli`や
あらゆるRedisクライアントライブラリがそのまま動作します。

```sh
cargo install kevy
kevy --port 6379 &
redis-cli -p 6379 SET hello world
redis-cli -p 6379 GET hello
```

## kevyとは

kevyは同一のエンジンから三つの形態で提供されます。

- **サーバー** — Redisワイヤ互換のデーモンです。RESP2を話し、98個の
  コマンドについてvalkey 9.1と返答をバイト単位で照合しています。
- **組み込みライブラリ** — `kevy-embedded`はネットワークのない同じ
  エンジンです。Rustバイナリに組み込んで`Store`を直接呼び出せます。
  純粋なRust、依存ゼロ。素の`core` KVからインデックス／レプリケー
  ションのフルサーフェスまでfeatureで段階化されており、両極端にも
  届きます——ブラウザ（npmの[`@goliajp/kevy`](docs/ja/wasm.md)）と
  655 KBのIoTビルド（[docs/iot.md](docs/iot.md)）です。
- **クライアント** — `kevy-client`（ブロッキング）と`kevy-client-async`
  （ランタイムごとにfeature flag一つ：tokio / smol / async-std）が
  あります。どちらもURLを受け取るため、同一のコードでTCPサーバー
  （`kevy://host:port`）にもプロセス内バス（`mem://name`）にも接続できます。

## kevy 4 — サービングエンジン、ここで確定

3.xでkevyは**serving engine**であることを宣言しました。従来なら
「RDS+前段キャッシュ」で構成していたアプリケーションの、プライマリ
ストアになるものです。完全なRedis互換に加えて、宣言型セカンダリ
インデックス（range / unique / CJK全文検索 / ベクトルANN。サーバー
サイドでBM25とKNNを融合するハイブリッド検索付き）とワンホップの
hydration、合成可能なビュー（仮想およびマテリアライズドtop-K）、
正確な復旧ポイントを持つCDCフィード（組み込みのoutbox）、チェック
サム検証付きの移行ツールチェーンが手に入ります。すべて
derived-by-construction（書き込みパスで維持され、ドリフトせず、
データから再構築可能）です。3.x本線ではマシン向けの顔——自己記述的な
verbコントラクト（`COMMAND DOCS`、自動生成リファレンス、`kevy-mcp`の
公式MCPサーバー）——と、可用性の一連の機能が加わりました。ハート
ビート/ACKによるラグの真値を持つストリーミングレプリケーション、
計画的なゼロロスの引き継ぎ（`FAILOVER`）とクォーラムによるクラッシュ
選出、そしてオプトインの整合性ラダー（`WAIT`、read-your-writes
トークン、上限付きステイルネス、クォーラムでフェンスされた書き込み）
です。詳細は[docs/availability.md](docs/availability.md)を参照して
ください。
4.0はこれらを確定させます。公開Rust APIは一度だけ整備され——エラー
型の統一（`KevyError`）、builderの統一、書き込み面の借用スライス化
（[docs/UPGRADING.md](docs/UPGRADING.md)）——以後は追加のみで凍結。
ランタイムはインスタンススコープになり、一つのプロセスで独立した
複数のkevyを走らせられます。そして同じエンジンがブラウザにも
エッジデバイスにも届きます（下の二つのセクション）。
主要な数値はすべてゲートされ、毎トレインで再計測されます。hydrated
行リストページのp99 < 1ms、書き込みファンアウトのp99 < 200µs、
ANN recall ≥ 0.9 — [設計マップ](docs/designing-on-kevy.md)、
[クックブック](docs/cookbook.md)、
[検証台帳](bench/VALIDATION-LEDGER.md)を参照してください。

## どれを使えばよいか

| 状況 | 選ぶもの |
|---|---|
| すでにRedisクライアントライブラリがあり、より速く軽いRedisが欲しい | サーバー（`kevy`） |
| Rustアプリがあり、別プロセスを起動したくない | 組み込みライブラリ（`kevy-embedded`） |
| RustからkevyまたはRedisサーバーと話したい | `kevy-client`（ブロッキング） |
| `tokio` / `smol` / `async-std`のRustで書いている | `kevy-client-async` |
| URL一つで組み込みとサーバーを切り替えられる同一コードが欲しい | `kevy-client` + `kevy-embedded` |

## インストール

```sh
# サーバー
cargo install kevy

# 組み込みライブラリ
cargo add kevy-embedded

# ブロッキングクライアント
cargo add kevy-client

# 非同期クライアント(ランタイムfeatureを一つ選ぶ)
cargo add kevy-client-async --features tokio
```

ビルド済みのサーバーバイナリは各[GitHub Release](https://github.com/goliajp/kevy/releases)
に添付されており、Linux x86_64、Linux aarch64、macOS Apple Siliconに
対応しています。マルチアーキテクチャのDockerイメージは
[Docker Hub](https://hub.docker.com/r/goliakk/kevy)と
[GitHub Container Registry](https://github.com/goliajp/kevy/pkgs/container/kevy)
の両方に公開されています。

```sh
docker run --rm -p 6379:6379 goliakk/kevy:latest
```

## クイックスタート

### サーバー

```sh
kevy --port 6379 &
redis-cli -p 6379 SET foo bar
redis-cli -p 6379 GET foo
```

設定の優先順位はCLIフラグ → 環境変数 → TOMLファイル → 組み込み
デフォルトの順です。注釈付きの完全なスキーマは
[`crates/kevy/kevy.toml.example`](crates/kevy/kevy.toml.example)にあります。

### 組み込みライブラリ

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;
store.set(b"key", b"value")?;
assert_eq!(store.get(b"key")?, Some(b"value".to_vec()));
# Ok::<(), kevy_embedded::KevyError>(())
```

`Store`は`Clone`であり、すべてのメソッドが`&self`を取るため、クローンを
スレッド間で自由に移動できます。ファイルバックドストアにするには
`Config::default().with_persist("/var/lib/myapp")`を使ってください。

### ブロッキングクライアント

```rust
use kevy_client::Connection;

let mut conn = Connection::connect("tcp://127.0.0.1:6379")?;
conn.set(b"k", b"v")?;
let v = conn.get(b"k")?;
assert_eq!(v.as_deref(), Some(&b"v"[..]));
# Ok::<(), kevy_client::KevyError>(())
```

同じURLの表面に`mem://app`を渡せばプロセス内のバックエンドに接続できる
ため、同じコードパスがテストでは組み込みストアに、本番ではネットワーク
経由のサーバーに対して動作します。

### 非同期クライアント

```rust,no_run
use kevy_client_async::AsyncConnection;

# async fn run() -> std::io::Result<()> {
let mut conn = AsyncConnection::connect("tcp://127.0.0.1:6379").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
# Ok(())
# }
```

`tokio`、`smol`、`async-std`のうちちょうど一つをCargo featureとして
選んでください。ゼロ個または二つ以上を選ぶとクレートはコンパイルを
拒否します。

## ブラウザで

kevyはブラウザの中で本物のストアとして動きます。npmパッケージ
[`@goliajp/kevy`](https://www.npmjs.com/package/@goliajp/kevy)は、
`wasm32-unknown-unknown`向けにコンパイルしたエンジンを手書きの
ESモジュールローダーに包んで出荷します——wasm-bindgenなし、境界の
両側とも依存ゼロ。六ファイル、パックで約165 KBです。

```sh
npm install @goliajp/kevy
```

```js
import { open } from "@goliajp/kevy";

const db = await open({ persist: { name: "app" } });
db.set("session", "abc123", { ttlMs: 60_000 });
db.subscribe("events", (payload) => { /* どのタブからでも発火 */ });
```

- **永続化**：書き込みはkevyのappend-onlyログとしてOPFSに流れます
  （IndexedDBフォールバック）。ネイティブkevyとバイト互換なので、
  ブラウザで書いたログはサーバーでそのままreplayできます。
- **タブ間pub/sub**：BroadcastChannelブリッジ経由。サーバーと同じ
  at-most-once契約です。
- **ストアとして速いべきところで速い**：ポイント読み取りは
  IndexedDBの77–86×、ポイント書き込みは166–189×、永続書き込みは
  そのレートの12.6–17.4×です
  （[`bench/WASM-BENCH.md`](bench/WASM-BENCH.md)）。

ローダーAPIとABI契約は[docs/ja/wasm.md](docs/ja/wasm.md)を、
[kevy.golia.jp/demo](https://kevy.golia.jp/demo/)ではこの全部が
ライブで動きます——永続化とタブ間pub/sub付きのブラウザREPL、
バックエンドなしです。

## エッジデバイスで（IoT）

同じ組み込みライブラリは下方向にもスケールします。`kevy-embedded`は
featureで段階化されており（`core` / `persist` / `index` / `text` /
`vector` / `replicate` / `listener`。デフォルトは全部入り）、`core`
段は**655 KB**のバイナリ（予算 ≤ 700 KB）にコンパイルされ、空の
ストアを2 MB未満のRSSで開きます——どちらのラインも
[`bench/iotgate.sh`](bench/iotgate.sh)がラチェットとして強制します。

- `aarch64`と`armv7`の静的muslクロスビルドはCIでゲートされています。
- さらにLinuxの外へ：五つの基盤クレート（`kevy-store`、`kevy-hash`、
  `kevy-bytes`、`kevy-map`、`kevy-madvise`）は`no_std` + `alloc`で
  ビルドでき、Cortex-Mターゲット（`thumbv7em-none-eabihf`）で
  実証済みです。

ティア表、予算、センサーキャッシュの例は[docs/iot.md](docs/iot.md)を
参照してください。

## パフォーマンス

ベアメタルベンチマークスイートからの代表的な抜粋です（16コアのLinux
マシン、サーバーとクライアントは互いに重ならないコアにピン留め、TCP
loopback、精密モードでCI95 < 1%）。詳細な手法、全ワークロード、注意点は
[`bench/REPORT.md`](bench/REPORT.md)にあり、すべての数値は
[`bench/`](bench/)のスクリプトから再現可能です。

| ワークロード | kevy | valkey 9.1 | 比率 |
|---|---:|---:|---:|
| `GET -c 50 -P 16` | 6.39 M/s | 2.13 M/s | **3.00×** |
| `SET -c 50 -P 16` | 6.39 M/s | 1.60 M/s | **4.00×** |
| Pub/subファンアウト（50 subs） | 23.1 M/s | 5.1 M/s | **4.52×** |
| 組み込み`get`（ヒット） | 9.0 M/s | — | （in-processのRedisは無い） |

同じ`GET -c 50 -P 16`の面を、同一マシン上で四つのエンジンと対戦
——kevyは6.39 M/sでそれぞれに対して（median-of-5。手法と
エンジンごとのサイクル記録は
[`bench/PERF-VERDICT-V4-T9.md`](bench/PERF-VERDICT-V4-T9.md)）：

| エンジン | kevyのリード |
|---|---:|
| valkey 9.1 | **3.00×** |
| redis 8 | **1.60×** |
| dragonfly | **3.60×** |

サービング面はredis-stack 7.4.7（RediSearch）と同一シード・
同一コーパスでrecallを揃えて比較（[`bench/PERF-LEDGER.md`](bench/PERF-LEDGER.md)）：

| クエリ種別 | kevy | RediSearch | 判定 |
|---|---:|---:|---|
| 全文検索（BM25 top-10） | 330 qps | 273 qps | **+21% qps**、p95同等 |
| ANN KNN @ recall 1.000 | 0.48 ms | 0.79 ms | **1.64×高速** |
| GROUP BY top-100 | 1.9 ms | 202.9 ms | **110×**（書き込み時集約） |
| 数値レンジ + hydrate | 0.19 ms | 0.43 ms | **2.3×** |

完全なサーバーはストリップ後768 KBのバイナリで、5 MB未満のRSSで
起動します。

**アップグレードは？** [docs/UPGRADING.md](docs/UPGRADING.md)が
両方のホップを一か所でカバーします——3.x → 4.0（ワイヤとディスクは
そのまま。Rust APIは一度だけ変わり、リネームごとに対照表と規則が
あります）と2.x → 3.x（バイナリ差し替え + 依存バージョンアップ）
です。アップグレード方向では、スナップショットとAOFはメジャーを
またいでそのまま読み込めます。

## 互換性

98個のコマンドがvalkey 9.1と返答をバイト単位で照合されており、Redisの
5つのデータ型（String、Hash、List、Set、Sorted Set）すべてに加えて
Streams、Pub/Sub（channel + pattern）、トランザクション（`MULTI` /
`EXEC` / `WATCH` / `UNWATCH`）、ブロッキングpop、および標準的な
操作・永続化系verbをカバーしています。コマンドの完全な一覧は
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md)にあります。

kevyに対してエンドツーエンドで検証済みのクライアントライブラリ：

| 言語 | ライブラリ | バージョン |
|---|---|---|
| Java | [Jedis](https://github.com/redis/jedis) | 5.x |
| .NET | [StackExchange.Redis](https://stackexchange.github.io/StackExchange.Redis/) | 2.x |
| Go | [go-redis](https://github.com/redis/go-redis) | v9 |
| Python | [redis-py](https://github.com/redis/redis-py) | 5.x |
| Python | [Celery](https://docs.celeryq.dev/) | 5.6 |
| Ruby | [Sidekiq](https://sidekiq.org/) | 6.5 |
| Node.js | [ioredis](https://github.com/redis/ioredis) | 5.7 |
| Node.js | [BullMQ](https://github.com/taskforcesh/bullmq) | 5.79 |
| Node.js | [Bee Queue](https://github.com/bee-queue/bee-queue) | 1.7 |
| Node.js | [node-redlock](https://github.com/mike-marcacci/node-redlock) | 5 |

いずれもデフォルトの`kevy --port 6379`インスタンスに対して無修正で
動作します。

## クレート

| クレート | 役割 |
|---|---|
| [`kevy`](crates/kevy) | サーバーバイナリとライブラリのエントリポイント |
| [`kevy-embedded`](crates/kevy-embedded) | Redis形状のRust APIを持つプロセス内KV |
| [`kevy-client`](crates/kevy-client) | ブロッキングRESPクライアント。サーバーまたはプロセス内バックエンドに対するURLファサード |
| [`kevy-client-async`](crates/kevy-client-async) | tokio / smol / async-std向けの`kevy-client`の非同期版 |
| [`kevy-cluster-rw`](crates/kevy-cluster-rw) | プライマリ書き込み・レプリカ読み取りのクライアントラッパー |
| [`kevy-cli`](crates/kevy-cli) | 運用CLI。バックアップ、リストア、スモークテスト |
| [`kevy-config`](crates/kevy-config) | CLI/env/fileの優先順位を持つTOML設定スキーマ |
| [`kevy-resp-client`](crates/kevy-resp-client) | 低レベルRESP2クライアントプリミティブ |
| [`kevy-bytes`](crates/kevy-bytes) | インラインまたはヒープの小文字列最適化付きownedバイト文字列 |
| [`kevy-hash`](crates/kevy-hash) | 単一信頼ドメインのキースペース向け高速非暗号学的ハッシュ |
| [`kevy-map`](crates/kevy-map) | SIMDグループスキャン付きSwiss-tableハッシュマップ |
| [`kevy-resp`](crates/kevy-resp) | ゼロアロケーションRESP2 / 3パーサ |
| [`kevy-ring`](crates/kevy-ring) | 上限付きロックフリーSPSCキュー |
| [`kevy-madvise`](crates/kevy-madvise) | Linux `MADV_HUGEPAGE`ラッパー。他環境ではno-op |
| [`kevy-uring`](crates/kevy-uring) | 純粋Rustのio_uringバインディング。liburingにリンクしない |
| [`kevy-geo`](crates/kevy-geo) | 地理空間コマンドプリミティブ |
| [`kevy-wasm`](crates/kevy-wasm) | ブラウザビルド。手書きC ABI + `@goliajp/kevy`ローダー |
| [`kevy-lua`](crates/kevy-lua) | Luaスクリプトブリッジ（[luna](https://github.com/goliajp/luna)ランタイムによる） |

残りのクレート（`kevy-store`、`kevy-rt`、`kevy-persist`、`kevy-sys`、
`kevy-elect`、`kevy-replicate`、`kevy-scope`、`kevy-lua-host`、
`kevy-chaos`、`kevy-bench`、`kevy-pubsub-bench`）はサーバーと組み込み
ライブラリのための内部インフラです。ワークスペースが再現可能にビルド
できるよう公開していますが、エンドユーザーは通常上記の表面に手を
伸ばすことになります。

**AIエージェント・ツール向け**：[`llms.txt`](llms.txt)（マシン
ファーストの索引）· [verbリファレンス](docs/verb-reference.md)
（全189 verb。サーバー自身のメタデータから生成され、`COMMAND DOCS`が
返すのと同じ行です）。

## トピックガイド

| トピック | ドキュメント |
|---|---|
| RDSワークロードのマッピング（SQL → kevy） | [`docs/rds-workloads.md`](docs/rds-workloads.md) |
| 移行プレイブックとツールチェーン | [`docs/migration.md`](docs/migration.md) |
| 設定チューニング | [`docs/ja/tuning.md`](docs/ja/tuning.md) |
| 永続化（AOF + RDB） | [`docs/ja/persistence.md`](docs/ja/persistence.md) |
| Pub/Sub | [`docs/ja/pubsub.md`](docs/ja/pubsub.md) |
| レプリケーション | [`docs/ja/replication.md`](docs/ja/replication.md) |
| クラスタモード | [`docs/ja/cluster.md`](docs/ja/cluster.md) |
| Luaスクリプト | [`docs/lua.md`](docs/lua.md) |
| Unixドメインソケット | [`docs/ja/uds.md`](docs/ja/uds.md) |
| 非同期クライアント | [`docs/ja/async.md`](docs/ja/async.md) |
| ブラウザ / WASM | [`docs/ja/wasm.md`](docs/ja/wasm.md) |
| IoTとfeatureティア | [`docs/iot.md`](docs/iot.md) |
| accept-shardサイジング | [`docs/accept-shards.md`](docs/accept-shards.md) |
| エラー応答リファレンス | [`docs/error-replies.md`](docs/error-replies.md) |

## スコープ外

kevyはやらないことについて正直です。チャーターにより、以下は永続的に
スコープ外で、追加する計画はありません。

- **AUTHとTLS。** kevyは信頼されたネットワークを前提とします。どちらかが
  必要なら、TLS終端のサイドカー（envoy、stunnel）と認証プロキシを前段に
  置いてください。
- **マルチDCのアクティブ-アクティブおよびDC間レプリケーション。** 単一DCのみです。
- **マルチデータベース`SELECT`。** サーバーごとに一つのキースペースです。
- **ACL。** 信頼ドメインは一つです。
- **gossipディスカバリとオンラインリシャーディング。** クラスタトポロジは
  宣言的で、リシャーディングはオフラインです。

これらのいずれかが必要なら、Redis Cluster、Valkey、またはホスト型KV
サービスが適しています。

## ビルドとテスト

```sh
cargo build --workspace --release
cargo test  --workspace
```

stable Rust 1.97.0、Rust 2024 editionです。Linux（`x86_64`、`aarch64`）と
macOSでビルドできます。`kevy-embedded`とその依存クロージャは
`wasm32-unknown-unknown`および`wasm32-wasip1`向けにもビルドできます。

## ロードマップと安定性

ワークスペースはv4.xラインに乗っています。永続化フォーマット、RESP
ワイヤプロトコル、公開Rust API、CLIフラグ、環境変数、TOMLスキーマ、
エビクションセマンティクスは各メジャーラインを通じて追加のみです。
さらにオンディスクフォーマットはメジャーをまたいで引き継がれます。
v2.0で書かれたスナップショットやAOFは、すべての3.x・4.xビルドで
そのまま読み込めます（[docs/UPGRADING.md](docs/UPGRADING.md)を参照）。追加
機能は既存コードを壊すことなくマイナーリリースで導入されます。完全な
安定性契約は
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md#v1x-stability-commitment)
にあります。

## ライセンス

MITまたはApache-2.0のいずれか、お好きな方でライセンスされています。

© 2026 GOLIA K.K.
