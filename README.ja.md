# kevy

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Rust stable](https://img.shields.io/badge/rust-stable-orange.svg)

純粋な Rust で書かれた、依存ゼロの Redis 互換キーバリューストアです。
スタンドアロンサーバとして、プロセス内ライブラリとして、あるいはその両方として
利用できます。どの形態でも RESP2 を話すため、`redis-cli` や任意の Redis
クライアントライブラリがそのまま動作します。

```sh
cargo install kevy
kevy --port 6379 &
redis-cli -p 6379 SET hello world
redis-cli -p 6379 GET hello
```

## kevy とは

kevy は同一のエンジンから三つの形態で提供されます。

- **サーバ** — Redis ワイヤ互換のデーモンです。RESP2 を話し、98 個のコマンドに
  ついて valkey 9.1 と返答をバイト単位で照合しています。
- **組み込みライブラリ** — `kevy-embedded` はネットワークのない同じエンジンです。
  Rust バイナリに組み込んで `Store` を直接呼び出せます。純粋な Rust、依存ゼロで、
  `wasm32` 向けにもビルドできます。
- **クライアント** — `kevy-client`(ブロッキング)と `kevy-client-async`
  (ランタイムごとに feature flag 一つ: tokio / smol / async-std)があります。
  どちらも URL を受け取るため、同一のコードで TCP サーバ(`kevy://host:port`)にも
  プロセス内バス(`mem://name`)にも接続できます。

## どれを使えばよいか

| 状況 | 選ぶもの |
|---|---|
| すでに Redis クライアントライブラリがあり、より速く軽い Redis が欲しい | サーバ(`kevy`) |
| Rust アプリがあり、別プロセスを起動したくない | 組み込みライブラリ(`kevy-embedded`) |
| Rust から kevy または Redis サーバと話したい | `kevy-client`(ブロッキング) |
| `tokio` / `smol` / `async-std` の Rust で書いている | `kevy-client-async` |
| URL 一つで組み込みとサーバを切り替えられる同一コードが欲しい | `kevy-client` + `kevy-embedded` |

## インストール

```sh
# サーバ
cargo install kevy

# 組み込みライブラリ
cargo add kevy-embedded

# ブロッキングクライアント
cargo add kevy-client

# 非同期クライアント(ランタイム feature を一つ選ぶ)
cargo add kevy-client-async --features tokio
```

ビルド済みのサーババイナリは各 [GitHub Release](https://github.com/goliajp/kevy/releases)
に添付されており、Linux x86_64、Linux aarch64、macOS Apple Silicon に対応しています。
マルチアーキテクチャの Docker イメージは [Docker Hub](https://hub.docker.com/r/goliakk/kevy)
と [GitHub Container Registry](https://github.com/goliajp/kevy/pkgs/container/kevy)
の両方に公開されています。

```sh
docker run --rm -p 6379:6379 goliakk/kevy:latest
```

## クイックスタート

### サーバ

```sh
kevy --port 6379 &
redis-cli -p 6379 SET foo bar
redis-cli -p 6379 GET foo
```

設定の優先順位は CLI フラグ → 環境変数 → TOML ファイル → 組み込みデフォルトの順です。
注釈付きの完全なスキーマは
[`crates/kevy/kevy.toml.example`](crates/kevy/kevy.toml.example) にあります。

### 組み込みライブラリ

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;
store.set(b"key", b"value")?;
assert_eq!(store.get(b"key")?, Some(b"value".to_vec()));
# Ok::<(), std::io::Error>(())
```

`Store` は `Clone` であり、すべてのメソッドが `&self` を取るため、クローンを
スレッド間で自由に移動できます。ファイルバックドストアにするには
`Config::default().with_persist("/var/lib/myapp")` を使ってください。

### ブロッキングクライアント

```rust
use kevy_client::Connection;

let mut conn = Connection::open("tcp://127.0.0.1:6379")?;
conn.set(b"k", b"v")?;
let v = conn.get(b"k")?;
assert_eq!(v.as_deref(), Some(&b"v"[..]));
# Ok::<(), std::io::Error>(())
```

同じ URL の表面に `mem://app` を渡せばプロセス内のバックエンドに接続できるため、
同じコードパスがテストでは組み込みストアに、本番ではネットワーク経由のサーバに
対して動作します。

### 非同期クライアント

```rust,no_run
use kevy_client_async::AsyncConnection;

# async fn run() -> std::io::Result<()> {
let mut conn = AsyncConnection::open("tcp://127.0.0.1:6379").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
# Ok(())
# }
```

`tokio`、`smol`、`async-std` のうちちょうど一つを Cargo feature として選んでください。
ゼロ個または二つ以上を選ぶとクレートはコンパイルを拒否します。

## パフォーマンス

ベアメタルベンチマークスイートからの代表的な抜粋です(16 コアの Linux マシン、
サーバとクライアントは互いに重ならないコアにピン留め、TCP loopback、精密モードで
CI95 < 1%)。詳細な手法、全ワークロード、注意点は
[`bench/REPORT.md`](bench/REPORT.md) にあり、すべての数値は
[`bench/`](bench/) のスクリプトから再現可能です。

| ワークロード | kevy | valkey 9.1 | 比率 |
|---|---:|---:|---:|
| `SET -c 1` | 94.7 k/s | 62.2 k/s | **1.52×** |
| `GET -c 1` | 97.3 k/s | 65.0 k/s | **1.50×** |
| `SET -c 50 -P 16` | 2.59 M/s | 1.82 M/s | **1.42×** |
| Pub/sub ファンアウト(50 subs) | 23.1 M/s | 5.1 M/s | **4.52×** |
| 組み込み `get`(ヒット) | 9.0 M/s | — | (プロセス内 Redis なし) |
| 組み込み `set`(上書き) | 7.0 M/s | — | (プロセス内 Redis なし) |

完全なサーバはストリップ後 768 KB のバイナリで、5 MB 未満の RSS で起動します。

## 互換性

98 個のコマンドが valkey 9.1 と返答をバイト単位で照合されており、Redis の 5 つの
データ型(String、Hash、List、Set、Sorted Set)すべてに加えて Streams、
Pub/Sub(channel + pattern)、トランザクション(`MULTI` / `EXEC` / `WATCH` /
`UNWATCH`)、ブロッキング pop、および標準的な操作・永続化系コマンドをカバーしています。
コマンドの完全な一覧は
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md) にあります。

kevy に対してエンドツーエンドで検証済みのクライアントライブラリ:

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

いずれもデフォルトの `kevy --port 6379` インスタンスに対して無修正で動作します。

## クレート

| クレート | 役割 |
|---|---|
| [`kevy`](crates/kevy) | サーババイナリとライブラリのエントリポイント |
| [`kevy-embedded`](crates/kevy-embedded) | Redis 形状の Rust API を持つプロセス内 KV |
| [`kevy-client`](crates/kevy-client) | ブロッキング RESP クライアント。サーバまたはプロセス内バックエンドに対する URL ファサード |
| [`kevy-client-async`](crates/kevy-client-async) | tokio / smol / async-std 向けの `kevy-client` の非同期版 |
| [`kevy-cluster-rw`](crates/kevy-cluster-rw) | プライマリ書き込み・レプリカ読み取りのクライアントラッパー |
| [`kevy-cli`](crates/kevy-cli) | 運用 CLI。バックアップ、リストア、スモークテスト |
| [`kevy-config`](crates/kevy-config) | CLI/env/file の優先順位を持つ TOML 設定スキーマ |
| [`kevy-resp-client`](crates/kevy-resp-client) | 低レベル RESP2 クライアントプリミティブ |
| [`kevy-bytes`](crates/kevy-bytes) | インラインまたはヒープの小文字列最適化付き owned バイト文字列 |
| [`kevy-hash`](crates/kevy-hash) | 単一信頼ドメインのキースペース向け高速非暗号学的ハッシュ |
| [`kevy-map`](crates/kevy-map) | SIMD グループスキャン付き Swiss-table ハッシュマップ |
| [`kevy-resp`](crates/kevy-resp) | ゼロアロケーション RESP2 / 3 パーサ |
| [`kevy-ring`](crates/kevy-ring) | 上限付きロックフリー SPSC キュー |
| [`kevy-madvise`](crates/kevy-madvise) | Linux `MADV_HUGEPAGE` ラッパー。他環境では no-op |
| [`kevy-uring`](crates/kevy-uring) | 純粋 Rust の io_uring バインディング。liburing にリンクしない |
| [`kevy-geo`](crates/kevy-geo) | 地理空間コマンドプリミティブ |
| [`kevy-lua`](crates/kevy-lua) | Lua スクリプトブリッジ([luna](https://github.com/goliajp/luna) ランタイムによる) |

残りのクレート(`kevy-store`、`kevy-rt`、`kevy-persist`、`kevy-sys`、
`kevy-elect`、`kevy-replicate`、`kevy-scope`、`kevy-lua-host`、`kevy-chaos`、
`kevy-bench`、`kevy-pubsub-bench`)はサーバと組み込みライブラリのための内部
インフラです。ワークスペースが再現可能にビルドできるよう公開していますが、
エンドユーザは通常上記の表面に手を伸ばすことになります。

## トピックガイド

| トピック | ドキュメント |
|---|---|
| 設定チューニング | [`docs/tuning.md`](docs/tuning.md) |
| 永続化(AOF + RDB) | [`docs/persistence.md`](docs/persistence.md) |
| Pub/Sub | [`docs/pubsub.md`](docs/pubsub.md) |
| レプリケーション | [`docs/replication.md`](docs/replication.md) |
| クラスタモード | [`docs/cluster.md`](docs/cluster.md) |
| Lua スクリプト | [`docs/lua.md`](docs/lua.md) |
| Unix ドメインソケット | [`docs/uds.md`](docs/uds.md) |
| 非同期クライアント | [`docs/async.md`](docs/async.md) |
| WebAssembly ビルド | [`docs/wasm.md`](docs/wasm.md) |
| accept-shard サイジング | [`docs/accept-shards.md`](docs/accept-shards.md) |
| エラー応答リファレンス | [`docs/error-replies.md`](docs/error-replies.md) |

## スコープ外

kevy はやらないことについて正直です。チャーターにより、以下は永続的にスコープ外で、
追加する計画はありません。

- **AUTH と TLS。** kevy は信頼されたネットワークを前提とします。どちらかが必要なら、
  TLS 終端のサイドカー(envoy、stunnel)と認証プロキシを前段に置いてください。
- **マルチ DC のアクティブ-アクティブおよび DC 間レプリケーション。** 単一 DC のみです。
- **マルチデータベース `SELECT`。** サーバごとに一つのキースペースです。
- **ACL。** 信頼ドメインは一つです。
- **gossip ディスカバリとオンラインリシャーディング。** クラスタトポロジは宣言的で、
  リシャーディングはオフラインです。

これらのいずれかが必要なら、Redis Cluster、Valkey、またはホスト型 KV サービスが
適しています。

## ビルドとテスト

```sh
cargo build --workspace --release
cargo test  --workspace
```

stable Rust 1.95、Rust 2024 edition。Linux(`x86_64`、`aarch64`)と macOS で
ビルドできます。`kevy-embedded` とその依存クロージャは
`wasm32-unknown-unknown` および `wasm32-wasip1` 向けにもビルドできます。

## ロードマップと安定性

ワークスペースは v2.x ラインに乗っています。永続化フォーマット、RESP ワイヤ
プロトコル、公開 Rust API、CLI フラグ、環境変数、TOML スキーマ、エビクション
セマンティクスはメジャーラインを通じて加算のみです。v2.0 で書かれたファイルは
それ以降のすべての v2.x ビルドで読み込め、追加機能は既存コードを壊すことなく
マイナーリリースで導入されます。完全な安定性契約は
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md#v1x-stability-commitment)
にあります。

## ライセンス

MIT または Apache-2.0 のいずれか、お好きな方でライセンスされています。

© 2026 GOLIA K.K.
