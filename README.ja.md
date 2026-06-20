# kevy

[English](README.md) · [简体中文](README.zh-CN.md) · **日本語**

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#ライセンス)
![Rust stable](https://img.shields.io/badge/rust-stable-orange.svg)

純 Rust・**ゼロ依存**・Redis 互換のキーバリュー・ストア。スタンドア
ロン・サーバーとしても、組込みライブラリとしても使えます。ハードウェア
の限界を引き出すことを目標に設計しました。

kevy は Redis ワイヤプロトコル(RESP2)を喋るため、`redis-cli` /
`valkey-cli` / 任意の Redis クライアント・ライブラリが**そのまま**接続
できます。内部はモダンな thread-per-core / shared-nothing アーキテクチャ
で、完全に Rust 実装 —— C に触れるのは避けられない OS システムコール境
界だけです。

```sh
cargo run -p kevy --bin kevy --release      # ループバック・AOF on・ポート 6004
redis-cli -p 6004 SET hello world
```

## kevy を選ぶ理由

- **速い** —— 高並行で valkey 9.1 のスループットの 2.7-3.0 倍、pub/sub
  ファンアウトで 2.7 倍、組込み時に **コア当たり ~9 M GET / 7 M SET**
  (数字は後述)。
- **小さい** —— 768 KB のサーバ・バイナリ、起動後 5 MB 未満の RAM 常駐。
  コンテナ sidecar、小規模 VM、エッジ箱に収まる。
- **モダンなアーキテクチャ** —— thread-per-core・shared-nothing・ホット
  パスは無ロック・Linux では io_uring。グローバル・ロックも GIL 様の
  ボトルネックもない。
- **サプライチェーン・リスクなし** —— デフォルトのサーバー /
  ブロッキング・クライアント / 組込みスタックは crates.io 依存ゼロ。
  ツリー全体が `std` + kevy 自家の crate で、C は OS システムコール境界
  だけ、1 つの crate に手書きでバインドされています。非同期クライアント
  (`kevy-client-async`)は唯一許された例外 —— オプトイン、lib 利用者
  のみ、文書で透明に記録。
- **互換性** —— RESP2 ワイヤプロトコル、valkey 9.1 と 98 コマンドのパリ
  ティ(パターン pub/sub と `WATCH`/`UNWATCH` 楽観 CAS を含む)、応答を
  バイト単位で照合。既存クライアントとツールはそのまま動きます。
- **レプリケーション**(v1.22)—— サーバー primary + N read replica +
  クォーラム・フェイルオーバー、**組込みノードがクラスタに参加可能**
  (read-replica またはプリフィックス単位の writer)。同一ワイヤプロト
  コルが貫通、宣言的なトポロジー一つ。
- **組込み可能** —— `kevy-store` は単なる Rust ライブラリ:ネットワーク
  も runtime もなし、`wasm32` ビルドもサポート。同じエンジンを自分の
  プロセスで動かせます。
- **非同期対応** —— `kevy-client-async`(v1.22)はブロッキング表面を
  1:1 ミラーし、`tokio` / `smol` / `async-std` をサポート。pipeline-first
  ビルダーで N コマンドを 1 TCP round-trip にまとめられます。
- **リソース適応型** —— メモリ無制限なら全速、制限ありなら優雅に縮退、
  境界では大声で拒否してデータを静かに破損させたりしない([詳細](#リソース適応型設計))。

スコープを正直に:kevy は**単一 DC** 設計、AUTH/TLS なし、公インターネッ
ト露出を想定しない(see [kevy をいつ使うか](#kevy-をいつ使うか))。
レプリケーションは単一 DC の primary-replica + クォーラム・フェイル
オーバー;クロス DC active-active・gossip・オンライン resharding・Raft
は明示的にスコープ外。

## 性能

下記の数字はすべて **16 コアのベアメタル Linux 機** (lx64)で計測、純
インメモリ、サーバー / クライアント / loadgen を別々の CPU に pin 済み。
すべての bench は [`bench/`](bench/) のスクリプトで再現可能;完全な
メソドロジー、注意事項、v0.2 → v1.22 の時系列叙述は
[`bench/REPORT.md`](bench/REPORT.md) にあります。

### サーバー・スループット(ネットワーク経由)

> valkey 9.1 を超えるのは床であって目標ではない —— kevy が狙うのは
> ハードウェア天井。

`redis-benchmark`、サーバーをコア 0-9 に pin、クライアントを別コアに
分離、各エンジンを**単独**実行(起動 → 2 回ウォーム ラン → 停止)し、
kevy の busy-poll が同居の競合相手を飢えさせないようにしました。各
エンジンは最速設定(valkey/redis は `--io-threads 10` 有効化):

| ワークロード | kevy 1.22 | valkey 9.1 (io-threads) | redis 7.4 (io-threads) |
|--------------|----------:|------------------------:|-----------------------:|
| **-c50 -P16 GET** | **6.0 M/s** | 2.0 M/s | 2.0 M/s |
| **-c50 -P16 SET** | **4.0 M/s** | 1.5 M/s | 1.5 M/s |
| **-c1 GET** | **68 k/s** | 60 k/s | 55 k/s |
| **-c1 SET** | **76 k/s** | 60 k/s | 54 k/s |

→ 高並行で kevy は **best-other 比 GET 3.0× / SET 2.7×**、
シングル接続シーケンシャル(busy-poll エンジンにとって最も厳しい
ワークロード)でも 1.13-1.26× リード。io_uring vs epoll は負荷形によ
る(io_uring は低並行で勝ち、epoll は -c50 -P16 で pipelining が
syscall 節約をならして追いつく)。再現方法:
[`bench/loopback_c50.sh`](bench/loopback_c50.sh) と
[`bench/loopback_c1.sh`](bench/loopback_c1.sh)。

io_uring の C 参照との比較:kevy の手書きバインディングで 148 ns の
nop round-trip 達成、対する liburing 2.9 は 152 ns —— Linux カーネル
の床、しかも liburing をリンクしていない。

### クラスタ・ルーティング(キー対応クライアント)

シングル・ポートのクライアントが間違った shard に着いたとき、内部の
クロス shard 転送ホップが発生します。クラスタ対応の
[`ClusterClient`](#クラスタモード単機キー対応ルーティング) は各キーを
所有 shard に直接ルーティングし、そのホップを完全に取り除きます。lx64
16 コア、サーバー/クライアント別コア、GET 並行 64:

| クライアント・パス | スループット | p99 レイテンシ |
|-------------------|-----------:|-------------:|
| シングル shard プロキシ(クロス shard ホップ) | 333 k/s | 3858 µs |
| **`ClusterClient`(ゼロ・ホップ)** | **533 k/s** | **260 µs** |

**スループット 1.6 倍、テール・レイテンシ約 15 倍低下** —— 純粋に転送
ホップを取り除いただけで、手書き生ルーターと比べて計測可能なオーバー
ヘッドなし。完全なメソドロジーは
[`docs/cluster.md`](docs/cluster.md)。

### クラスタモード(レプリケーション + フェイルオーバー + 組込み参加)

v1.22 で v3-cluster トラックを閉じました。kevy ノードは **primary**
として、適用された各 mutation を N replica にストリーミング配信でき、
**replica** として primary をミラーすることもできます;**組込みノード
がクラスタに参加可能**(read-replica としても、プリフィックス単位の
writer としても);`kevy-elect` が primary DOWN 時に**クォーラムに基づく
自動フェイルオーバー**を実行します。コンパニオン・クライアント
`kevy-cluster-rw` は書き込みを primary に送り、読み取りを replica で
round-robin します。

```toml
# primary
[replication]
role = "primary"
listen_port_base = 16004

# replica
[replication]
role = "replica"
upstream = "primary.example:16004"
```

```sh
# 実行時に Redis 互換コマンドで再ターゲット / 昇格。
redis-cli -p 6004 REPLICAOF primary.example 16004
redis-cli -p 6004 REPLICAOF NO ONE
redis-cli -p 6004 ROLE
```

フェーズ別カバレッジ(v1.22 ですべてマージ):
- **Phase 1**(v1.18):per-shard ワイヤ backlog + listener、フォール
  バック replica 向けスナップショット出荷、動的 REPLICAOF /
  `REPLICAOF NO ONE` 再ターゲット + 降格、`ROLE` / `INFO replication`
  ライブ状態、`kevy-cluster-rw` 読み書き分離クライアント。
- **Phase 1.5**(v1.19):`kevy-elect` クォーラムに基づく自動 primary
  フェイルオーバー(ハートビートによる DOWN 検出、OFFER/ACCEPT/
  ANNOUNCE、最高 offset 当選)。
- **Phase 2**(v1.22):**組込みノードが read-replica としてクラスタに
  参加可能** —— `kevy-embedded` を組み込むアプリケーションがサーバー
  primary のレプリケーション・ストリームを購読し、キースペースをプロ
  セス内でミラーします。読み取りはネットワーク round-trip ゼロ;
  ローカル書き込みは `READONLY` で返ります。
- **Phase 3**(v1.22):**スコープ別マルチ writer** —— `[cluster] scopes
  = "app:billing:=embed-a,app:catalog:=embed-b"` でプリフィックス単位の
  writer 所有権を宣言;間違ったプリフィックスを受け取ったノードは
  `-MISDIRECTED writer is <host:port>` を返します。オペレータ起動の
  `MOVE-SCOPE` が quiesce-window プロトコルでプリフィックスを移行します。

スコープ外(永久に対応しない):オーバーラップする multi-master、
クロス DC active-active / CRDT、Raft、gossip ディスカバリ、オンライン
resharding、AUTH/TLS。

サーバー + クライアントの完全なレシピは
[`docs/replication.md`](docs/replication.md) と
[`docs/cluster.md`](docs/cluster.md) を参照。

### 組込みスループット(プロセス内、ネットワークなし)

[`kevy-embedded`](crates/kevy-embedded) をアプリに drop して `Store`
を直接呼ぶ —— ソケットなし、RESP 解析なし、reactor なし。lx64 でプロ
セス内 bench(1 M ops、12 バイトキー、16 バイト値):

| 操作 | レイテンシ | スループット |
|------|---------:|----------:|
| `get`(ヒット) | 111 ns | **9.0 M ops/s** |
| `get`(ミス) | 24 ns | **42.2 M ops/s** |
| `set`(上書き) | 143 ns | **7.0 M ops/s** |
| `incr` | 169 ns | 5.9 M ops/s |
| `del` | 183 ns | 5.5 M ops/s |

再現:
`cargo run -p kevy-embedded --example embed_throughput --release`。

#### 同じ Rust caller、4 バックエンド

公平な比較:**同じ Rust プログラム** でバックエンドだけを切り替える
—— これが実アプリが実際に見る数字です。シングル接続、シーケンシャル、
N=200k SET + N GET;3 つのサーバー列はすべて **同じ**
`kevy_client::Connection` の RESP パスを通り、URL だけが異なります:

| バックエンド(同 Rust caller) | SET ops/s | GET ops/s |
|------------------------------|----------:|----------:|
| **kevy 1.22 embed** | **10.10 M** | **13.76 M** |
| **kevy 1.22 server (io_uring)** | **63.5 k** | **64.4 k** |
| valkey 9.1 server @ localhost | 54.6 k | 53.8 k |
| redis 7.4 server @ localhost | 62.3 k | 61.7 k |

embed は同じ kevy を TCP-loopback で呼ぶより **SET ~160×、GET ~214×
速い** —— これが「ソケットなし、プロトコルなし、reactor なし」が組込
めるアプリにもたらす定量化された節約です。これは embed が駆動する
kevy-vs-valkey/redis スループット主張では **ありません** —— valkey と
redis にプロセス内モードがないので、構造的なギャップは避けられません。
再現:
`cargo run -p kevy-embedded --example embed_vs_server --release
--kevy-port 7011 --valkey-port 7012 --redis-port 7013 -N 200000`。

### Pub/sub ファンアウト(サーバー・モード)

1 パブリッシャ → 50 サブスクライバ、200 000 メッセージ、16 バイト
ペイロード、ウォーム実行。TCP / RESP パス上で kevy は最速のブローカー
です:

| システム | 配信 msg/s | vs valkey |
|---------|---------:|--------:|
| Aeron 1.45(IPC、共有メモリ) | 84 M | 12.4× |
| **kevy 1.22** | **18.5 M** | **2.72×** |
| ZeroMQ 4.3.5 | 9.4 M | 1.38× |
| redis 7.4 | 8.9 M | 1.31× |
| valkey 9.1 | 6.8 M | 1.00× |
| Zenoh 1.9 | 2.9 M | 0.43× |

Aeron の共有メモリ IPC は構造的な天井(カーネル・ネットワーク・スタッ
クを介さない);TCP ブローカー間で kevy がリード —— 同じトランスポート
の ZeroMQ の 2 倍、しかも非ブローカーの ZeroMQ ダイレクト・メッセー
ジングをも上回ります。Pub/sub は**サーバーモード**機能;組込みライブ
ラリは純粋なキーバリューです。方法 + 6-way ハーネス:
[`bench/pubsub-compare/`](bench/pubsub-compare/)。

### バイナリ・サイズとメモリ

| | |
|---|---|
| サーバー・バイナリ(`release`、stripped) | **768 KB** |
| サーバー・バイナリ(`release-min`、`opt-level="s"`) | **640 KB** |
| アイドル RSS(デフォルト 16 スレッド) | **4.9 MB** |
| アイドル RSS(`--threads 1`) | **2.5 MB** |
| キー当たりメモリ(8.6 M キー時) | ~190 B(キー + 値 + テーブル・オーバヘッド) |

`SmallBytes` は ≤ 22 B のペイロードをヒープ・アロケーションなしで
インライン化します。完全な kevy サーバーはサブ MB のバイナリで、起動後
5 MB 未満の RAM 常駐です。

ホスト自体のチューニング(CPU ピン留め、AOF、io_uring、Spectre 緩和)
は [`docs/ja/tuning.md`](docs/ja/tuning.md) を参照してください。

## クイック・スタート

### インストール

ビルド済みの `kevy` サーバー・バイナリは各
[GitHub Release](https://github.com/goliajp/kevy/releases) に添付されて
います。対応プラットフォーム:

| プラットフォーム | archive |
|----------------|---------|
| Linux x86_64 (glibc) | `kevy-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 (glibc) | `kevy-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` |
| macOS aarch64 (Apple Silicon) | `kevy-vX.Y.Z-aarch64-apple-darwin.tar.gz` |

ソースからビルド:

```sh
git clone https://github.com/goliajp/kevy
cd kevy
cargo build -p kevy --bin kevy --release
./target/release/kevy --port 6004
```

### Docker で起動

```sh
# メインライン・イメージ:distroless ベースの kevy サーバー。
docker run --rm -p 6004:6004 ghcr.io/goliajp/kevy:1.22 \
  kevy --bind 0.0.0.0 --port 6004
```

イメージは `kevy` と `kevy-cli`(redis-cli 代替)を含み、RESP `PING`
応答を監視する HEALTHCHECK が設定されています。

### サーバーとして

```sh
# デフォルト:ループバック、AOF オフ、ポート 6004。
cargo run -p kevy --bin kevy --release
```

設定ファイル(任意):

```toml
# kevy.toml
port = 6004
bind = "127.0.0.1"
threads = 8           # shard 数、デフォルトは CPU 数
persist_dir = "/var/lib/kevy"
aof = true
```

`kevy --config kevy.toml` でロード、または完全に環境変数で:
`KEVY_BIND`、`KEVY_PORT`、`KEVY_THREADS`、`KEVY_AOF`、`KEVY_IO_URING`。

### クラスタモード(単機、キー対応ルーティング)

```sh
kevy --threads 8 --cluster          # メイン・ポート 6004、shard ポート 6005-6012
redis-cli -c -p 6005 SET foo bar    # MOVED を自動追従
```

Rust 呼び出し元向けに、[`kevy-client`](crates/kevy-client) 1.11 は型付
き `ClusterClient` を提供します —— 一度トポロジーを発見してから各キー
を所有 shard に直接ルーティング、`-MOVED` なし・転送ホップなし(上記
の **スループット 1.6×・テール・レイテンシ 15×** 勝利):

```rust
// Cargo.toml: kevy-client = "1.11"
use kevy_client::ClusterClient;

let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;  // 任意の shard ポートを seed に
cc.set(b"user:42", b"alice")?;                            // CRC16 slot でルーティング
let v = cc.get(b"user:42")?;
let removed = cc.del(&[b"a", b"b", b"c"])?;               // 複数キーは shard を跨ぐ可能性あり
# Ok::<(), std::io::Error>(())
```

string / hash / list / set / sorted-set / del / exists / dbsize /
flushall / ping / publish をラップします;完全ガイド・コマンド表・
same-slot 規則は [`docs/cluster.md`](docs/cluster.md)。一つのクライ
アントが押し込む負荷でホップが目立つときに使う;通常はシンプルな単一
ポートの `Connection` で正しく、より簡単です。

Redis Cluster のスーパーセット注記(単機クラスタモード —— gossip /
MIGRATE-ASK / オンライン resharding なし):クロス slot 複数キー
コマンド(`MGET`、`SUNION`、トランザクション、ブロッキング・ファン
アウト)は `-CROSSSLOT` で失敗するのではなく実行されます;キース
ペース全域ビュー(`KEYS`、`SCAN`、`DBSIZE`)は各ポートでも全キース
ペース範囲を保ちます。既存データ・ディレクトリのクラスタモード切り
替えは起動時に一度キーを re-home します(元ファイルは
`*.premigration.<ts>` にバックアップ)。

primary + replicas + 自動フェイルオーバーの多ノード・クラスタには、
上の**クラスタモード(レプリケーション + フェイルオーバー + 組込み参加)**
セクションを参照 —— v1.22 で server-as-replica、embed-as-replica、
スコープ別マルチ writer、クォーラム昇格を提供しています。

### 非同期ランタイム・クライアントとして

`tokio` / `smol` / `async-std` 上で動くアプリは、ブロッキング・クライ
アントの非同期ミラーが使えます:

```rust
// Cargo.toml: kevy-client-async = { version = "1", features = ["tokio"] }
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;

// N コマンドを 1 TCP round-trip にパイプライン化:
let replies = conn.pipeline()
    .set(b"a", b"1").get(b"a").incr(b"hits")
    .run(&mut conn).await?;
# Ok::<(), std::io::Error>(())
```

ランタイム feature(`tokio` / `smol` / `async-std`)を**正確に 1 つ**
選択する必要があります;ゼロまたは 2 つ以上だとコンパイル・エラー。
ブロッキング [`kevy-client`](crates/kevy-client) はデフォルトであり、
0 依存を維持 —— async は opt-in。完全ガイド + ランタイム比較 +
パイプライン判断:[`docs/async.md`](docs/async.md)。

### 組込みライブラリとして

```rust
// Cargo.toml: kevy-embedded = "1.4"
use kevy_embedded::{Config, Store};

let s = Store::open(Config::default().without_aof())?;
s.set(b"key", b"value")?;
assert_eq!(s.get(b"key")?, Some(b"value".to_vec()));
# Ok::<(), std::io::Error>(())
```

`Store` はどこでも `&self` —— スレッド間で自由に clone できます、shard
内部で自分でロックします。永続化ファイル・ストレージには
`Config::default().with_persist("/var/lib/myapp")`。組込みをサーバー
primary の read-replica として動かす(v1.22)には、
[`docs/replication.md`](docs/replication.md) を参照。

## リソース適応型設計

kevy のリソース・ルールは一つ:**空きがあれば全速、なければ生き延びる、
境界では硬く拒否し、大声で失敗する —— 決して静かに腐らせない**。これ
はエンジンを貫通します:

- **無制限 = 全速**。`maxmemory = 0`(デフォルト)時、アカウンティング・
  オーバーヘッドはコンパイル時に単一分岐の判定で除去されます。設定し
  ていない制限のコストは一切払いません。
- **制限あり = 優雅な eviction**。`maxmemory` + ポリシー(LRU / LFU /
  Random / TTL、計 8 種)を設定すると、書き込みはサンプルされたキーを
  **制限の 5% 下**まで evict します —— 次の書き込みがすぐ eviction に
  再突入しないよう余裕を残します。
- **境界 = 大声で拒否、腐らせない**。`NoEviction`(デフォルト・ポリシー)
  下、予算を超える書き込みは実行前に Redis 古典の `OOM` エラーで拒否
  されます —— ホットパス上 O(1) 事前チェック。メモリを**増やす**動詞
  のみゲートで、縮小(`DEL` / `LPOP` / `SREM` / `EXPIRE` / …)と
  `FLUSH*` は常に通るため、満杯インスタンスから常に回復可能。
- **能力は降格、クラッシュしない**。io_uring は起動時に検出され、古い
  カーネル / seccomp サンドボックスでは **epoll にフォールバック**
  (`KEVY_IO_URING` で強制可能)。`wasm32` 組込みビルドはホストからの
  クロック投入 + サーフェスの縮小で動き、ビルド失敗にはしません。
  非ループバック `--bind` は **警告を出力**(kevy には AUTH/TLS なし)
  し、静かに露出しません。

クラスタ対応の [`ClusterClient`](#クラスタモード単機キー対応ルーティング)
はクライアント側で同じ哲学に従います:負荷でホップが目立つときは接続
数を費やしてスキップ、そうでないときは単純な単一ポートに留まります。

## kevy をいつ使うか

✅ 向いている:
- 内部キャッシュ / セッション・ストア / レート制限 / リーダーボード /
  カウンタ / pub/sub バス
- エッジ箱 / VM sidecar / コンテナ内で同ホスト・プロセス間連携
- Rust アプリケーションがプロセス内 KV を必要とする(オプションでロー
  カル・クラスタ参加も)
- より大きな redis 互換 KV の前に置く高速で信頼できるバックエンド・
  ベースライン

❌ 向いていない:
- 公インターネットまたはマルチテナント SaaS デプロイ(AUTH/TLS なし、
  永久にない)
- クロス DC active-active レプリケーション / 強整合性要件(単一 DC
  primary-replica + クォーラム・フェイルオーバーがスコープ)
- 永続データベースの ACID / 全文検索 / 時系列 / 関係クエリ(スコープ外
  —— 専用ストレージを使う)

## Crates

主要な crates.io 公開 crate:

| crate | 用途 |
|-------|------|
| [`kevy`](crates/kevy) | サーバー・バイナリ `kevy` と `kevy-cli` |
| [`kevy-embedded`](crates/kevy-embedded) | 組込み `Store` + Config + replica/writer 参加 |
| [`kevy-client`](crates/kevy-client) | ブロッキング・クライアント + `ClusterClient` |
| [`kevy-client-async`](crates/kevy-client-async) | 非同期クライアント(tokio/smol/async-std) |
| [`kevy-store`](crates/kevy-store) | 低レベル shard `Store`(組込み用、config 組立てなし) |
| [`kevy-resp`](crates/kevy-resp) | RESP2/3 編復号 |
| [`kevy-resp-client`](crates/kevy-resp-client) | ブロッキング RESP クライアント基盤 |
| [`kevy-scope`](crates/kevy-scope) | スコープ別 writer 所有権(P3) |
| [`kevy-replicate`](crates/kevy-replicate) | レプリケーション・ストリーム・プロトコル + クライアント |
| [`kevy-elect`](crates/kevy-elect) | クォーラム・フェイルオーバー・プロトコル |
| [`kevy-cluster-rw`](crates/kevy-cluster-rw) | 読み書き分離 + scope ルーティング・クライアント |

その他のサポート crate(`kevy-bytes` / `kevy-hash` / `kevy-map` /
`kevy-rt` / `kevy-persist` / `kevy-sys` / `kevy-uring` / `kevy-madvise` /
`kevy-ring` / `kevy-config` / `kevy-geo`)もすべて crates.io 公開で、
組み合わせ用に個別に取り込めます。

## 組込み ↔ サーバー、1 つの URL

[`kevy-client`](crates/kevy-client) は両バックエンドを同じ URL イン
ターフェース下に隠すので、ビジネス・コードは**URL 文字列の切替え**で
プロセス内組込み / TCP サーバーを切替えられます:

| URL | バックエンド |
|-----|------------|
| `mem://` | プロセス内組込み、純メモリ、匿名 bus |
| `mem://<name>` | プロセス内組込み、純メモリ、**名前付き共有 bus**(同名で別 open でも同じ pub/sub バスを見る) |
| `file:///abs/path` | プロセス内組込み + 永続化(AOF) |
| `kevy://host[:port][/db]` | TCP RESP、kevy ネイティブ別名 |
| `redis://host[:port][/db]` | TCP RESP、標準 Redis URL |
| `tcp://host[:port]` | TCP RESP、生アドレス(SELECT ヘッダなし) |

```rust
use kevy_client::Connection;

let url = std::env::var("MY_KEVY_URL").unwrap();
let mut c = Connection::open(&url)?;
c.set(b"hello", b"world")?;
assert_eq!(c.get(b"hello")?, Some(b"world".to_vec()));
# Ok::<(), std::io::Error>(())
```

dev/test に `mem://`、staging に `file:///tmp/staging`、prod に
`kevy://prod-host:6004` —— ビジネス・コードは不変。

## コマンド

[`docs/COMMANDS.md`](docs/COMMANDS.md) を参照。要点:**string / hash /
list / set / sorted-set / パターン pub/sub** 完全;**トランザクション**
(`MULTI` / `EXEC` / `WATCH`)完全;**streams**(`XADD` / `XREAD` /
`XLEN`)サブセット;**キースペース通知**(`__keyspace@*__:*` /
`__keyevent@*__:*`)完全。

`SCAN` / `HSCAN` / `SSCAN` / `ZSCAN` は完全な cursor 実装。`OBJECT
ENCODING` は各型で valkey が期待する文字列(`ziplist` / `hashtable` /
`intset` / …)にフォールバックして、データ形状検出ツールとの互換性を
保ちます。

非サポート:`CLUSTER`(サブセット —— [`docs/cluster.md`](docs/cluster.md)
参照)、`SCRIPT EVAL`(luna runtime 待ち)、`MODULE`、`MIGRATE` /
`ASK`、`AUTH` / `ACL`。

## ビルド & テスト

```sh
# ワークスペース全体のコンパイル + ユニット・テスト。
cargo build --workspace --release
cargo test --workspace --release

# Bench(ローカル;公平な数字には lx64 級のマシンが必要)。
bash bench/loopback_c50.sh
bash bench/loopback_c1.sh
bash bench/pubsub-compare/run.sh
```

CI は stable Rust(MSRV pin なし)+ `-D warnings` clippy + miri
(advisory FFI は miri 下で short-circuit)+ Docker イメージ・ビルド +
マルチターゲット・クロス・ビルドを実行します。

## ロードマップと安定性

- **v1.22**(2026-06-20、リリース済み)—— v3-cluster バンドル:組込み
  read-replica + スコープ別マルチ writer + 非同期クライアント。詳細
  は [`CHANGELOG.md`](CHANGELOG.md)。
- **v1.22.x 後続**(時期未定)—— マルチ shard upstream レプリカ、
  末尾まで進んだ backlog オフセット・スナップショット ingest、
  F4 フォールバック・パスでの writer 自動 reclaim。
- **v2-8 Lua**(luna runtime 待ち)—— `SCRIPT EVAL` / `EVALSHA` を
  自家 Lua 5.5 runtime で実装、kevy のプラグインとして提供、サーバー
  内にインタプリタを詰め込まない。

API 安定性:公開された crate は semver(メジャー 1.x)に従う;デフォル
ト・サーバーは後方互換のワイヤ・プロトコルを維持;組込み API のサー
フェス追加はマイナー・バージョンで行う。

## ライセンス

MIT または Apache-2.0、お好みで。
