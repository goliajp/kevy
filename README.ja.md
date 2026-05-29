# kevy

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#ライセンス)
![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)

純 Rust・**ゼロ依存**・Redis 互換のキーバリューサーバー ——
ハードウェアが許す限りの速度で動くように作られています。

kevy は Redis ワイヤプロトコル（RESP2）を話すため、`redis-cli`、
`valkey-cli`、そしてあらゆる Redis クライアントライブラリが**変更なしで**
接続できます。内部のエンジンは完全に Rust で書かれたモダンな
thread-per-core・shared-nothing 設計で、触れる C は避けられない OS の
システムコール境界だけです。

```sh
cargo run -p kevy --bin kevy --release      # loopback のみ、AOF 有効、ポート 6004
redis-cli -p 6004 SET hello world
```

## パフォーマンス

> valkey 9.1 を超えるのは下限であって目標ではありません ——
> kevy が狙うのはハードウェアの天井です。

専用の 16 コア Linux マシンで測定（サーバーはコア 0–9、クライアントは
独立したコア）：

| 指標 | kevy (io_uring) | valkey 9.1 (io-threads) | 倍率 |
|------|----------------:|------------------------:|-----:|
| **-c50 SET / 秒** | **4.0 M** | 1.5 M | **2.67×** |
| **-c50 GET / 秒** | **4.0 M** | 1.7 M | **2.33×** |
| -c1 SET / 秒 | 88 k | 58 k | 1.52× |
| -c1 GET / 秒 | 80 k | 65 k | 1.25× |

io_uring の C リファレンス実装との比較：**kevy の手書き io_uring
バインディングは nop ラウンドトリップ 148 ns、liburing 2.9 は 152 ns** ——
liburing をリンクせず、Linux カーネルの底値に達しています。各コア
ライブラリ crate のベンチマークは、最良のオープンソース
Rust / Go / C / C++ 競合と同等かそれ以上のノイズフロア水準です（8 / 8）。

詳細な手法と再現手順は [`bench/REPORT.md`](bench/REPORT.md) を参照してください。

## kevy を選ぶ理由

- **crates.io 依存ゼロ。** `std` と kevy 自身の crate のみ。すべての
  hashmap・hash 関数・プロトコルパーサは Rust 自前実装で、唯一の C は OS
  境界（socket、epoll / io_uring、mmap）を単一 crate 内で `unsafe extern "C"`
  により手書きバインドしたものです。
- **Thread-per-core・shared-nothing。** コアごとに 1 つの reactor と 1 つの
  keyspace シャード、ホットパスにロックなし。コア間はメッセージパッシングで
  協調します。
- **そのまま Redis 互換。** RESP2 ワイヤプロトコル、valkey 9.1 と 94 コマンド
  の同等性 —— redis-rs、go-redis、jedis、ioredis などがコード変更なしで
  動作します。
- **永続化。** スナップショット + 追記ファイル（AOF）、`appendfsync` は
  `always` / `everysec` / `no` に対応し、Redis のセマンティクスに一致します。
- **モダンなデータ構造**で、Redis のレガシーエンコーディングではありません
  —— 5 つのデータ型をすべてゼロから再実装しています。

## クイックスタート

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
