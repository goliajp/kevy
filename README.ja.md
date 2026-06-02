# kevy

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#ライセンス)
![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)

純 Rust・**ゼロ依存**・Redis 互換のキーバリューストア —— スタンドアロン
サーバーとしても、組み込みライブラリとしても使え、ハードウェアが許す限りの
速度で動くように作られています。

kevy は Redis ワイヤプロトコル（RESP2）を話すため、`redis-cli`、
`valkey-cli`、そしてあらゆる Redis クライアントライブラリが**変更なしで**
接続できます。内部のエンジンは完全に Rust で書かれたモダンな
thread-per-core・shared-nothing 設計で、触れる C は避けられない OS の
システムコール境界だけです。

```sh
cargo run -p kevy --bin kevy --release      # loopback のみ、AOF 有効、ポート 6004
redis-cli -p 6004 SET hello world
```

## kevy を選ぶ理由

- **速い** —— 高並行時のスループットは valkey 9.1 の 2.3〜2.7×、pub/sub
  ファンアウトで 2.7×、組み込み時はコアあたり約 1,800 万 ops/s（数値は下記）。
- **フットプリントが小さい** —— 768 KB のサーバーバイナリ、起動後の
  メモリは 5 MB 未満。コンテナのサイドカー、小型 VM、エッジ機器に収まります。
- **モダンなアーキテクチャ** —— thread-per-core・shared-nothing、ホット
  パスにロックなし、Linux では io_uring。グローバルロックも GIL 的な
  ボトルネックもありません。
- **サプライチェーンリスクなし** —— crates.io 依存ゼロ。依存ツリー全体が
  `std` と kevy 自身の crate のみで、唯一の C は OS のシステムコール境界を
  単一 crate で手書きバインドしたもの。kevy 以外に監査すべきものはありません。
- **そのまま互換** —— RESP2 ワイヤプロトコル、valkey 9.1 と 94 コマンドの
  同等性、応答をバイト単位で照合済み。既存のクライアントとツールがそのまま
  動きます。
- **組み込み可能** —— `kevy-store` は普通の Rust ライブラリ：ネットワーク
  なし、ランタイムなし、`wasm32` 向けにもビルドできます。同じエンジンを
  あなたのプロセス内で。

スコープについては正直に：kevy は**シングルノード**です —— レプリケーション、
クラスタリング、AUTH/TLS、インターネットへの直接公開は行いません
（[kevy を使うべき場面](#kevy-を使うべき場面)を参照）。

## パフォーマンス

以下のすべての数値は、**ベアメタルの Intel Core i7-10700K**（8 コア /
16 スレッド、3.8 GHz ベース / 5.1 GHz ブースト）、62 GB RAM、Linux 6.12.90
上、インメモリで測定したものです。各ベンチマークは [`bench/`](bench/) の
スクリプトで再現できます。詳細な手法と注意点は
[`bench/REPORT.md`](bench/REPORT.md) にあります。

### サーバースループット（ネットワーク経由）

> valkey 9.1 を超えるのは下限であって目標ではありません ——
> kevy が狙うのはハードウェアの天井です。

`redis-benchmark`、各サーバーはコア 0–9 に、クライアントは独立したコアに
ピン留めし、それぞれ単独で実行。各エンジンは最速の構成を使用（kevy：-c50 は
io_uring、-c1 は epoll；valkey/redis：io-threads）：

| ワークロード | kevy | valkey 9.1 | redis 7.4 |
|------------|-----:|-----------:|----------:|
| **-c50 -P16 GET** | **4.4 M/s** | 2.5 M/s | 2.3 M/s |
| **-c50 -P16 SET** | **4.7 M/s** | 1.9 M/s | 2.0 M/s |
| **-c1 GET** | **86 k/s** | 65 k/s | 48 k/s |
| **-c1 SET** | **72 k/s** | 63 k/s | 54 k/s |

io_uring の C リファレンス実装との比較：kevy の手書きバインディングは nop
ラウンドトリップ 148 ns、liburing 2.9 は 152 ns —— liburing をリンクせず、
Linux カーネルの底値に達しています。
[`bench/loopback_c50.sh`](bench/loopback_c50.sh) と
[`bench/loopback_c1.sh`](bench/loopback_c1.sh) で再現できます。

### 組み込みスループット（プロセス内、ネットワークなし）

[`kevy-store`](crates/kevy-store) をアプリに組み込み、直接呼び出します ——
socket なし、RESP パースなし、reactor なし。シングルコア、`Store` API：

| 操作 | レイテンシ（中央値） | スループット |
|------|-----------------:|-----------:|
| `get`（ヒット） | 54 ns | 約 1,850 万 ops/s |
| `get`（ミス） | 14 ns | — |
| `set`（上書き） | 76 ns | 約 1,300 万 ops/s |
| `incr` | 86 ns | — |

これは**ネットワークサーバーのコアあたりスループットの約 3 倍**です ——
組み込みパスはワイヤ層全体をスキップします。
`cargo run -p kevy-store --example bench_keyspace --release` で再現できます。

### Pub/sub ファンアウト（サーバーモード）

1 パブリッシャー → 50 サブスクライバー、200,000 メッセージ、16 バイト
ペイロード。kevy は TCP / RESP パス上で最速の broker です：

| システム | 配信 msg/s | valkey 比 |
|---------|----------:|----------:|
| Aeron 1.45（IPC、共有メモリ） | 26.5 M | 3.90× |
| **kevy** | **18.2 M** | **2.68×** |
| ZeroMQ 4.3.5 | 9.3 M | 1.37× |
| redis 7.4 | 8.5 M | 1.25× |
| valkey 9.1 | 6.8 M | 1.00× |
| Zenoh 1.9 | 2.7 M | 0.40× |

Aeron の共有メモリ IPC は構造的な上限です（カーネルのネットワークスタックを
通らない）。TCP broker の中では kevy が先頭 —— 同じトランスポートで ZeroMQ の
2 倍です。Pub/sub は**サーバーモード**の機能で、組み込みライブラリは純粋な
キーバリューです。手法と 6-way 比較ハーネスは
[`bench/pubsub-compare/`](bench/pubsub-compare/) にあります。

### バイナリサイズとメモリ

| | |
|---|---|
| サーバーバイナリ（`release`、strip 済み） | **768 KB** |
| サーバーバイナリ（`release-min`、`opt-level="s"`） | **640 KB** |
| アイドル時 RSS（デフォルト 16 スレッド） | **4.9 MB** |
| アイドル時 RSS（`--threads 1`） | **2.5 MB** |
| キーあたりメモリ（800 万キー時） | 約 190 B（key + value + テーブルオーバーヘッド） |

`SmallBytes` は ≤ 22 B のペイロードをインライン化し、ヒープ割り当てゼロ。
完全な kevy サーバーは 1 MB 未満のバイナリで、起動後 5 MB 未満の RAM に
収まります。

## クイックスタート

### インストール

各 [GitHub Release](https://github.com/goliajp/kevy/releases) には、
プリビルド済みの `kevy` サーバーバイナリが添付されています。サポートする
ターゲット：

| プラットフォーム | アーカイブ |
|------------------|------------|
| Linux x86_64 | `kevy-<TAG>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 | `kevy-<TAG>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `kevy-<TAG>-aarch64-apple-darwin.tar.gz` |
| Windows x64 | `kevy-<TAG>-x86_64-pc-windows-msvc.zip` |
| Windows arm64 | `kevy-<TAG>-aarch64-pc-windows-msvc.zip` |

ワンライナーでのインストール（Linux / macOS、ターゲットを選択）：

```sh
TAG=v1.0.0-rc4
TARGET=x86_64-unknown-linux-gnu      # または aarch64-unknown-linux-gnu, aarch64-apple-darwin
curl -L "https://github.com/goliajp/kevy/releases/download/$TAG/kevy-$TAG-$TARGET.tar.gz" | tar -xz
sudo install "kevy-$TAG-$TARGET/kevy" /usr/local/bin/kevy
kevy --port 6004
```

各アーカイブには `kevy` バイナリ、`kevy.toml.example`、`README.md`、
2 つのライセンスファイルが同梱されています。アセットごとに対応する
`.sha256` も公開されます。あるいは下記のとおりソースからビルドできます。

### サーバーとして

```sh
# デフォルト設定でビルドして実行（loopback のみ、AOF 有効、ポート 6004）
cargo run -p kevy --bin kevy --release

# または TOML 設定ファイルを使用
cp crates/kevy/kevy.toml.example ./kevy.toml
cargo run -p kevy --bin kevy --release -- --config ./kevy.toml

redis-cli -p 6004 SET foo bar
redis-cli -p 6004 GET foo
```

優先順位は CLI 引数 > 環境変数 > TOML ファイル > 組み込みデフォルト：

```sh
kevy --bind 0.0.0.0 --port 7000 --threads 4 --dir /var/lib/kevy
# 同等の環境変数：KEVY_BIND  KEVY_PORT  KEVY_THREADS  KEVY_DIR  KEVY_AOF
```

注釈付きの完全な設定 schema は
[`crates/kevy/kevy.toml.example`](crates/kevy/kevy.toml.example) を参照してください。

### 組み込みライブラリとして

```rust
// Cargo.toml: kevy-store = "0.1"
use kevy_store::Store;

let mut s = Store::default();
s.set(b"key".to_vec(), b"value".to_vec(), None, false, false);
assert_eq!(s.get(b"key").unwrap().unwrap(), b"value");
```

## kevy を使うべき場面

kevy v1.0 は以下の 4 つのシナリオで本番運用に対応しています：

1. **ローカル開発** —— `cargo run -p kevy` と好みの Redis クライアント。
2. **docker-compose 内部** —— ネットワーク内で `KEVY_BIND=0.0.0.0`。信頼境界は
   docker ネットワークそのものです。
3. **組み込みライブラリ** —— [`kevy-store`](crates/kevy-store) をアプリに直接
   組み込む：ネットワークなし、reactor なし。
4. **キャッシュ** —— 本物のデータベースを前段に置き、kevy が TTL +
   `maxmemory` + LRU / LFU エビクションでホットデータを保持します。

**設計上、対象外：** レプリケーション、クラスタリング、AUTH / TLS、そして
インターネットへの直接公開。HA / マルチホストには Kubernetes StatefulSet か
サイドカープロキシのパターンを使ってください。範囲選択の根拠と 94 コマンドの
同等性テーブルは [`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md) にあります。

## Crates

kevy は小さく再利用可能な crate 群として提供されます —— 8 つの公開ライブラリ
に加え、サーバー内部のコンポーネント：

| crate | 役割 |
|-------|------|
| [`kevy-bytes`](crates/kevy-bytes) | インライン／ヒープの small-string 最適化を備えた所有バイト列 |
| [`kevy-hash`](crates/kevy-hash) | 単一信頼ドメインの keyspace 向け高速非暗号 hash |
| [`kevy-map`](crates/kevy-map) | SIMD グループスキャン付き Swiss-table hashmap |
| [`kevy-resp`](crates/kevy-resp) | ゼロアロケーションの RESP2 / 3 パーサ |
| [`kevy-ring`](crates/kevy-ring) | 有界ロックフリー SPSC キュー |
| [`kevy-madvise`](crates/kevy-madvise) | Linux `MADV_HUGEPAGE` ラッパー、他環境では no-op |
| [`kevy-uring`](crates/kevy-uring) | 純 Rust の io_uring バインディング、liburing 不使用 |
| [`kevy-resp-client`](crates/kevy-resp-client) | ブロッキング RESP2 クライアント |
| `kevy-config` · `kevy-store` · `kevy-rt` · `kevy-persist` | 設定、keyspace、ランタイム、永続化 |
| `kevy-sys` | 唯一の libc 境界（サーバー内部） |
| `kevy` | サーバーバイナリ |

## コマンド

5 つの Redis データ型 —— **String、Hash、List、Set、Sorted Set** —— に加え、
**pub/sub**、**トランザクション**（`MULTI` / `EXEC` / `DISCARD`）、永続化
（`SAVE` / `BGSAVE` / `BGREWRITEAOF`）、運用コマンド（`INFO` / `CONFIG` /
`CLIENT` / …）。マルチキーコマンドと pub/sub はコアごとのシャードをまたいで
動作し、`WRONGTYPE` の挙動は Redis と同じです。

valkey 同等性の注記付き完全な 94 コマンド一覧は
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md) にあります。

## ビルドとテスト

```sh
cargo build --workspace --release
cargo test  --workspace
bash bench/run.sh        # valkey との比較ベンチ（Linux + Docker）
```

安定版 Rust 1.95、Rust 2024 edition。Linux（`x86_64`、`aarch64`）と macOS で
ビルドできます。`kevy-embedded` とその依存閉包は
`wasm32-unknown-unknown` / `wasm32-wasip1` 向けにもビルドできます ——
WebAssembly の手順は [`docs/wasm.md`](docs/wasm.md) を参照してください。

## ロードマップと安定性

kevy は現在 **v1.0.0-rc** のフィードバック期間です。v1.x が維持を約束する
すべて —— 永続化フォーマット、RESP ワイヤプロトコル、公開 Rust API、CLI
引数、環境変数、TOML schema、エビクションのセマンティクス —— は v1.x
ライン全体で**追加のみ（後方互換）**です：v1.0 が書き出したファイルは、後の
どの v1.x ビルドでも読み込めます。完全な安定性契約は
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md#v1x-stability-commitment)
にあります。

## ライセンス

あなたの選択により、**MIT** または **Apache-2.0** のいずれかのデュアル
ライセンスで提供されます。© 2026 GOLIA K.K.
