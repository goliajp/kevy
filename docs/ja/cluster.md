# クラスタ

kevy のクラスタ機能には独立した 2 つのレイヤがあります — **シングルノードのマルチシャード公開**(1 プロセスで、すべてのシャードが Redis Cluster を話す)と、**マルチノードのレプリケーション + スコープ付きマルチライター**(プライマリ、レプリカ、組み込み、クォーラムフェイルオーバー) — どちらか、両方、あるいはどちらも使わない、を選べます。

## 2 レイヤの一目見て

**シングルノードクラスタモード。** 1 つの kevy プロセスがキー空間を N シャードに分割し、各シャードを決定論的なシャード別ポートで仮想クラスタノードとして公開します。`CLUSTER SLOTS / SHARDS / NODES` は本物の CRC16 パーティションを報告します。キーを意識するクライアント(`redis-cli -c`、`redis-benchmark --cluster`、市販のクラスタ対応ライブラリ、同梱の [`ClusterClient`](https://github.com/goliajp/kevy/blob/master/crates/kevy-client/examples/cluster.rs))は各キーをハッシュして所有シャードに直接接続します。利得は機械的です — サーバー側のクロスシャードホップが消えると、スループットとテール遅延に直接効きます。

**マルチノードクラスタ。** kevy サーバーは**プライマリ**として 1 つ以上の**レプリカ**(kevy サーバー、またはプロセス内の [`kevy-embedded`](https://github.com/goliajp/kevy/tree/master/crates/kevy-embedded) ストア)に書き込みログをストリーミングできます。プライマリは**スコープ付き書き込み**をプレフィックスごとに委譲もできます。`[cluster] scopes` が、`app:billing:*` の書き込みはどのノード、`app:auth:*` はどのノード、と宣言します。違うノードに着地した書き込みは `-MISDIRECTED writer is <host:port>` を受け取り、クライアントが追従します。[`kevy-elect`](https://github.com/goliajp/kevy/tree/master/crates/kevy-elect) はクォーラムハートビートを提供し、ライターを DOWN とフラグして宣言済みのフォールバックを昇格させます。オペレータ発行の `MOVE-SCOPE` は quiesce 窓のもとでプレフィックスを移行します。

## このドキュメントが必要になるとき

| 状況 | 使うもの |
|-----------|-----------|
| 1 プロセス、キーを意識するクライアント、クロスシャードホップを消したい | シングルノードクラスタモード + `ClusterClient` |
| 1 ホスト上で市販の Redis Cluster ツールと互換にしたい | シングルノードクラスタモード |
| 別マシンまたはプロセス内から hot reads を返したい | マルチノード: プライマリ + レプリカ(またはレプリカとして組み込む) |
| キープレフィックスでパーティション分けされた複数ライターを別ホストで | マルチノード: スコープ付きマルチライター |
| 人を介さずにライタークラッシュを乗り切りたい | マルチノード: `kevy-elect` + スコープフォールバック |
| 1 プロセス、低負荷、普通のクライアント | どちらも不要 — デフォルトのプロキシポートで十分 |

2 レイヤは合成可能です。クラスタモードのプライマリが N シャードを広告し、各レプリカも N シャードで動き、ルーティングクライアントが両者を繋ぎます。

---

# レイヤ 1 — シングルノードクラスタモード

## 中心となる考え方

普通の kevy プロセスは 1 つのポートで全コマンドを受け、所有シャードへミスルーティングされたキーを内部で転送します。その転送は正しいのですが、ホットパスでは p99 遅延を支配し、スループットの上限を作ります。クラスタモードは各シャードを独自のポートで公開します。キーを意識するクライアントは CRC16-XMODEM でキーをハッシュし、`CLUSTER SLOTS` から所有シャードを引き、そこへ直接接続します — 転送なし、`-MOVED` なし。

```
                  ┌─────────────────────────────────────────┐
                  │            kevy プロセス (1 ホスト)     │
                  │                                         │
  メインポート ─▶ │  6004  ── proxy: 転送 または -MOVED ──▶ │
                  │                                         │
  シャードポート▶ │  6005  ── shard 0   (slots     0– 4095) │
                  │  6006  ── shard 1   (slots  4096– 8191) │
                  │  6007  ── shard 2   (slots  8192–12287) │
                  │  6008  ── shard 3   (slots 12288–16383) │
                  └─────────────────────────────────────────┘
```

シャード `i` は常に `port_base + 1 + i` に bind します(`port_base` は TOML で上書き可)。メインポートはクラスタを話さないクライアント向けにプロキシ挙動を保ち、シャード別ポートは間違った所有者にキーが来ると `-MOVED <slot> <host:port>` を返します。

キー空間全体を対象とするコマンド(`KEYS`、`SCAN`、`DBSIZE`、`FLUSHALL`)はどのポートでもキー空間全体を対象に保たれます — kevy が内部でファンアウトし、クライアントが面倒を見る必要はありません。

## 有効化

```toml
# kevy.toml
port = 6004

[cluster]
enabled   = true
# port_base = 6004   # 既定は `port`。シャードは port_base + 1 + i に存在。
```

CLI / 環境変数で同等:

```sh
kevy --port 6004 --threads 8 --cluster      # シャードポート 6005..6012
KEVY_CLUSTER=1 kevy --port 6004 --threads 8
```

データディレクトリをクラスタモードに入れる/出すと、起動時に一度だけキーが再ホームされます。元ファイルは `*.premigration.<ts>` としてバックアップされます。

## Rust から `ClusterClient` を使う

```toml
[dependencies]
kevy-client = "*"
```

```rust
use kevy_client::ClusterClient;

// 任意のクラスタポートにシード接続。トポロジは CLUSTER SLOTS で発見し、
// シャードあたり 1 本のコネクションを開く。
let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;

cc.set(b"user:42", b"alice")?;
let v = cc.get(b"user:42")?;            // user:42 の所有シャードへルーティング
let n = cc.incr(b"counter")?;

// マルチキーの DEL/EXISTS — キーごとにルーティングして合算。
let removed = cc.del(&[b"a", b"b", b"c"])?;
# Ok::<(), std::io::Error>(())
```

実行可能なシード例は [`crates/kevy-client/examples/cluster.rs`](https://github.com/goliajp/kevy/blob/master/crates/kevy-client/examples/cluster.rs)、ベンチマークは [`crates/kevy-client/examples/cluster_bench.rs`](https://github.com/goliajp/kevy/blob/master/crates/kevy-client/examples/cluster_bench.rs) にあります。

### ルーティングがクロスシャードホップを消す仕組み

1. **発見。** `connect` は seed に `CLUSTER SLOTS` を送り、各シャードの `[start, end, host, port]` を読み、16384 エントリの `slot → shard-index` テーブルを構築します。テーブルはサーバー広告レンジから作られるので、クライアントはパーティショニング演算を再実装しません。
2. **ルーティング。** すべてのシングルキー・コマンドは `key_hash_slot(key)`(`{hashtag}` があればその CRC16-XMODEM、なければキー全体)を計算し、そのスロット所有者のコネクションへ直接送ります。
3. **必要な所だけファンアウト。** `dbsize`、`flushall` 等のクラスタ全体コマンドはサーバー側で処理。クライアントは 1 回呼ぶだけです。

16 コアの lx64 マシン上、並行 64 の GET でクロスシャードホップを消すと測定スループットは 333 k ops/s から 533 k ops/s(1.6×)に上がり、p99 は 3858 µs から 260 µs(約 15× 低いテール)に下がりました。`cargo run -p kevy-client --release --example cluster_bench` で再現できます。

> ホップのコストはクリーンなマシン上で負荷を掛けたときだけ可視化します。小さい同居クラウド VM ではスケジューリングノイズに埋もれて差が見えません。

### クロススロットのマルチキー・コマンド

Redis Cluster と違い、kevy はシングルノードクラスタでマルチキー・コマンド(`MGET`、`MSET`、`SUNION`、トランザクション、ブロッキングファンアウト)がシャードをまたいでも `-CROSSSLOT` を返**しません**: サーバーがシャードをまたいで要求を満たします。kevy は単一マシン上で Redis Cluster のスーパーセットです — どの Redis Cluster クライアントも動き、加えて `-CROSSSLOT` で当たっていた面もそのまま動きます。アトミック性のためにデータを同居させたいときは `{hashtag}` の共有が依然として正しい道具ですが、正しさのためにはもう必須ではありません。

### クラスタポートでサポートされる `CLUSTER` コマンド

| コマンド | 挙動 |
|---------|-----------|
| `CLUSTER SLOTS` | 本物のパーティション。シャードごとに `[start, end, host, port]` 1 行。 |
| `CLUSTER SHARDS` | 同データの新しい形。プライマリノードのみ。 |
| `CLUSTER NODES` | フラットなテキスト manifest、シャードごとに 1 行。ID はシャードインデックスから派生。 |
| `CLUSTER MYID` | 呼び出しに答えたシャードの決定論的 ID。 |
| `CLUSTER KEYSLOT <key>` | `{hashtag}` またはキー全体の CRC16-XMODEM。 |
| `CLUSTER COUNTKEYSINSLOT <slot>` | 所有シャードのインデックスを歩いて生カウント。 |
| `CLUSTER COUNT-FAILURE-REPORTS <id>` | 常に 0 — このレイヤには故障検出器がない。 |
| `CLUSTER INFO` | `cluster_enabled:1`、`cluster_state:ok`、スロットカバレッジを返す。 |
| `CLUSTER RESET`、`CLUSTER FORGET`、`CLUSTER MEET`、`CLUSTER FAILOVER`、`MIGRATE`、`ASK` | 未実装 — *スコープ外* 節を参照。 |

### 生のルーティングヘルパーへのフォールバック

```rust
// 任意のシングルキー・コマンドを所有シャードへルーティング。
let reply = cc.request_keyed(b"mykey", &[b"STRLEN".to_vec(), b"mykey".to_vec()])?;
// キーなしコマンドは任意のシャードへ。
let reply = cc.request_unkeyed(&[b"PING".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

`ClusterClient` は文字列、ハッシュ、リスト、セット、ソート済みセット、pub/sub、マルチキー `DEL` / `EXISTS` の共通動詞をラップします。Pub/sub はプロセス全体に届きます: 任意のポートの `Subscriber` は、どのシャードが `PUBLISH` を受けたかに関係なく、発行されたすべてのメッセージを見ます。

---

# レイヤ 2 — マルチノードクラスタ

## プライマリとレプリカ

kevy サーバーはプライマリ(デフォルト)、プライマリの書き込みログをミラーするレプリカ、または両方を同時に(カスケード)動作できます。プライマリは専用のレプリケーションリスナーを開きます。レプリカは接続し、最後に適用したオフセットを渡し、ストリーミングされたフレームをローカルシャードに適用します。

```toml
# primary.toml
port = 6004

[replication]
listen_port = 16004        # プライマリはここでログをストリーミング
```

```toml
# replica.toml
port = 6004

[replication]
upstream    = "primary.local:16004"
replica_id  = "replica-eu-1"           # レプリカごとに安定。再起動を超えて維持。
# reconnect_min_ms = 100               # バックオフ範囲
# reconnect_max_ms = 5000
```

完全なサーバー側セマンティクス — バックログサイジング、スナップショット取り込み、カスケード — は [`docs/replication.md`](replication.md) にあります。本書にとって重要な事実は、同じワイヤプロトコルがクラスタモードのレプリケーションも運ぶことです: `[cluster] enabled = true` で動くプライマリは N シャードぶんの書き込みをストリーミングし、同じシャード数で動くレプリカがシャード対シャードで適用します。

## レプリカとして組み込む

[`kevy-embedded`](https://github.com/goliajp/kevy/tree/master/crates/kevy-embedded) のストアはプライマリに直接 subscribe して、ネットワークホップなしでプロセス内 reads を返せます。書き込みは `READONLY` でローカル拒否されます。

```rust
use kevy_embedded::Store;

// インメモリレプリカ、AOF off、デフォルト再接続(100 ms → 5 s)。
let replica = Store::open_replica("primary.local:16004")?;

let v = replica.get(b"hello")?;
assert!(replica.set(b"k", b"v").is_err());      // READONLY
# Ok::<(), std::io::Error>(())
```

チューニング:

```rust
use std::time::Duration;
use kevy_embedded::{Config, Store};

let cfg = Config::default()
    .with_replica_upstream("primary.local:16004")
    .with_replica_id("backup-svc-region-a")
    .with_replica_reconnect(Duration::from_millis(50), Duration::from_secs(10));
let replica = Store::open(cfg)?;
# Ok::<(), std::io::Error>(())
```

ハンドシェイクは `REPLICATE FROM <last-applied-offset> ID <replica_id>` を送ります。プライマリはオフセットを ack し、フレームをストリーミングします。最後の `Store` クローンが drop されるとランナースレッドは join され、プライマリはクリーンな FIN を観測してスロットを解放します。組み込み上で `PUBLISH` はローカルに許可(pub/sub はプロセスローカル)ですが、キー空間自体は読み取り専用のままです。

## スコープ付きマルチライター

スコープ付きマルチライターはキープレフィックスごとに書き込みをノードに振り分けます。各ノードは静的 config から所有テーブル全体を知っており、非所有者に着地した書き込みは `-MISDIRECTED writer is <host:port>` を返してクライアントが正しいノードに再試行します。

```toml
# 同じ config ブロックを全メンバに置く。
[cluster]
node_id = "embed-billing-1"
peers   = "embed-billing-1@10.0.0.1:6004,server-eu-1@10.0.0.2:6004,reader-1@10.0.0.3:6004"

# prefix=writer[|fallback]、カンマ区切り。
# 最初の `=` がプレフィックスと所有者指定を分けるので、`app:billing:`(`:` 含む)でも OK。
scopes  = "app:billing:=embed-billing-1|server-eu-1, app:auth:=embed-auth-1"

elect_port_base = 16100    # kevy-elect がここで listen
```

`peers` は `<node_id>@<host>:<port>` エントリのフラットな文字列です — ネスト構造なし、テンプレート化が容易。`scopes` は `prefix=writer[|fallback]` をカンマ区切りで parse します。スコープを所有しないノードは単に書き込みを転送し、スコープを所有するノードはそれを受け、他は拒否します。

reads はスコープ所有とは独立です — データを持つどのノード(典型的にはリードレプリカ)でも返せます。スコープの仕組みは書き込みの帰属だけのためにあります。

### スコープライターとして組み込む

```rust
use kevy_embedded::{Config, Store};

let writer = Store::open(
    Config::default().with_embed_writer("0.0.0.0:6105")
)?;

// ローカル書き込みは組み込みのレプリケーションソースのバックログに流れる;
// リーダーは kevy_replicate::ReplicaClient で 0.0.0.0:6105 に接続する。
writer.set(b"app:billing:invoice:42", b"...")?;
# Ok::<(), std::io::Error>(())
```

組み込みは `with_embed_writer` に渡したアドレスでレプリケーションリスナーを公開します。他のノードはサーバープライマリから引くのと全く同じようにそこからログを引きます。

## `kevy-elect` クォーラムフェイルオーバー

`kevy-elect` はクラスタの全メンバーが走らせるサイドカーハートビートです。各ノードは elect ポート(`elect_port_base + node_index`)に HB を投げ、各ノードは最近誰が生きていたかのスライディングウィンドウを保持します。ピアの最後の HB が `down_after`(デフォルト 5 s)を過ぎると、そのピアは `down_peers` 入りします。スコープの宣言済みフォールバックは各受領書き込みで `down_peers` を参照します: ライターが DOWN なら、フォールバックは自分をアクティブ所有者として扱って書き込みを受領し、他のノードのその後の書き込みはフォールバックに MISDIRECT されます。元ライターの HB が戻ると `down_peers` を抜け、次回の判断で暗黙にフォールバックは退きます。

| ノブ | 意味 | デフォルト |
|------|---------|---------|
| `node_id` | このノードの安定識別子(`<scope_owner>` 参照とマッチ) | 必須 |
| `peers` | 全クラスタメンバーの `<node_id>@<host>:<port>` リスト | 必須 |
| `elect_port_base` | ローカル elect サイドカーが bind する UDP ポート | `16100` |
| `hb_interval_ms` | HB 発行周期 | `500` |
| `down_after_ms` | HB なしでピアが DOWN とされるまでの時間 | `5000` |

### 手動 rejoin リカバリ

元ライターが DOWN だった時間が長く、フォールバックが書き込みを受領していた場合、その書き込みはフォールバック上にしかありません。スコープの元ライターを再有効化する前に: ライターを止め、フォールバックのデータディレクトリをライターのものへコピーし、再起動します。これでコンセンサスなしの契約を保てます — シャドウ書き込みなし、二重受領なし。

---

# `MOVE-SCOPE`

`MOVE-SCOPE` は有界な quiesce 窓のもとで、あるライターから別のライターへプレフィックスを移行します。これはオペレータ発行で、現ライター上で走ります。

```
MOVE-SCOPE <prefix> from <from-node-id> to <to-node-id>
```

ステップごと:

1. 現ライターは `<prefix>` のローカル状態を MIGRATING に flip します。以降そのプレフィックス配下のキーへの書き込みは `-QUIESCED migrating to <to-host:port>` を返します。クライアントは少し待って再試行します。
2. ライターはプレフィックスのキー空間スライスをシリアライズし、`MOVE-SCOPE-INGEST <prefix> <bulk>` でターゲットのデータポートへ送ります。
3. ターゲットから `+OK` を受け取ると、ライターはローカルに移行をコミットします。以降そのプレフィックスへのソースの書き込みは `-MISDIRECTED writer is <to-host:port>` を返します。
4. 他のクラスタメンバーは、オペレータが新 config を push して再起動するまで静的 `scopes` どおりにルーティングを続けます。

移動中に見える 2 つのワイヤ応答:

| 応答 | 意味 |
|-------|---------|
| `-MISDIRECTED writer is <host:port>` | 書き込みが非所有者に着地。指定されたホストに再試行。 |
| `-QUIESCED migrating to <host:port>` | MOVE-SCOPE 窓中の一時的なもの。少し待って再試行。 |

クラスタ対応クライアントは `-MISDIRECTED` でキーごとのターゲットをキャッシュして透過再試行します。`-QUIESCED` では数百ミリ秒くらい眠ってから再試行するのが適切です。

移動の中断はソースライターに復元します。ターゲットには部分適用状態が残りません。

---

# 設定リファレンス

## シングルノードクラスタモード

| TOML | CLI | 環境変数 | デフォルト | 意味 |
|------|-----|-----|---------|---------|
| `[cluster] enabled` | `--cluster` | `KEVY_CLUSTER=1` | `false` | 各シャードをシャード別ポートで公開。 |
| `[cluster] port_base` | `--cluster-port-base` | `KEVY_CLUSTER_PORT_BASE` | `port` の値 | シャード `i` は `port_base + 1 + i` を bind。 |

## レプリケーション(プライマリ側)

| TOML | CLI | 環境変数 | デフォルト |
|------|-----|-----|---------|
| `[replication] listen_port` | `--replication-listener` | `KEVY_REPLICATION_LISTEN_PORT` | 未設定(off) |

## レプリケーション(レプリカ側)

| TOML | CLI | 環境変数 | デフォルト |
|------|-----|-----|---------|
| `[replication] upstream` | `--replicate-from` | `KEVY_REPLICATE_FROM` | 未設定 |
| `[replication] replica_id` | `--replica-id` | `KEVY_REPLICA_ID` | ホスト名から派生 |
| `[replication] reconnect_min_ms` | | | `100` |
| `[replication] reconnect_max_ms` | | | `5000` |

## スコープ付きマルチライター + elect

| TOML | 意味 |
|------|---------|
| `[cluster] node_id` | このノードの安定識別子。 |
| `[cluster] peers` | 全クラスタメンバーの `<node_id>@<host>:<port>` リスト。 |
| `[cluster] scopes` | `prefix=writer[\|fallback]` エントリ、カンマ区切り。 |
| `[cluster] elect_port_base` | ローカル elect サイドカーが bind する UDP ポート。 |
| `[cluster] hb_interval_ms` | HB 発行周期(デフォルト `500`)。 |
| `[cluster] down_after_ms` | HB なしでピアが DOWN とされるまでの時間(デフォルト `5000`)。 |

---

# トレードオフと限界

- **シングルノードクラスタモードは 1 プロセスです。** 買えるのはクライアント側のキー・ルーティングであり、ホストレベルの耐障害性ではありません。そちらが必要ならレプリカを足してください。
- **プロキシポートは生き続けます。** クラスタを話さないクライアントは引き続き動作し、正しいまま — ただしクロスシャードホップ付きで。
- **トポロジは静的です。** `peers` と `scopes` は起動時に config から読まれます。変更は「新 config を push して再起動」です。設計上、ゴシップはありません。
- **`MOVE-SCOPE` はプレフィックスの書き込みを quiesce します。** 窓はスライス送出時間で有界です。LAN 経由の GB クラスのスコープなら 1 桁秒です。それより遥かに大きいプレフィックスはメンテナンス窓に合わせて実施してください。
- **スコープライターとしての組み込みはサービス形状のワークロード**(請求サービス、認証サービス)を想定し、マルチ TB のデータセットを想定していません。
- **フォールバック受領後の手動 rejoin リカバリ。** 再有効化前にフォールバックのデータディレクトリをライターへコピー。自動コンセンサスキャッチアップはありません。

---

# 設計上のスコープ外

- AUTH と TLS — デプロイのエッジ(サイドカー、メッシュ、LB)で扱い、kevy では扱わない。
- マルチ DC アクティブ・アクティブと CRDT。
- Raft、Paxos、その他キー空間の下のコンセンサスログ。
- ゴシップベースの発見 — `peers` は静的。
- オンラインリシャーディング、`MIGRATE`、`ASK` リダイレクト。
- 所有が重なるマルチマスター — 各プレフィックスには常にちょうど 1 つのライターしかない。

これらは追加されません。シンプルさが機能です。

---

# FAQ

**レプリケーションを使うのにクラスタモードは必要ですか?**
いいえ。シングルノードクラスタモードとレプリケーション/マルチノードレイヤは独立です。非クラスタなプライマリは非クラスタなレプリカを持てます。クラスタなプライマリはクラスタなレプリカを持てます。合成可能ですが、どちらも他方を必須としません。

**クラスタモードの kevy に対して標準のクラスタ対応クライアント(Lettuce、ioredis、redis-py-cluster)は使えますか?**
はい。`CLUSTER SLOTS / SHARDS / NODES` は本物のパーティションを広告し、間違ったシャードヒットでは `-MOVED` が出ます。これはそれらライブラリが依存する面そのものです。クライアントのルーティングがシャードに届くよう、メインプロキシポートではなくシャード別ポートに揃えてください。

**シングルノードクラスタモードでシャードをまたぐマルチキー・コマンドはどうなりますか?**
成功します。kevy はクロススロットの `MGET`、`MSET`、`SUNION`、トランザクション、ブロッキングファンアウトを `-CROSSSLOT` を返さずサーバー側で実行します。アトミック性が重要なケースで `{hashtag}` の同居は依然有用ですが、もはや正しさのための要件ではありません。

**オペレータなしでライタークラッシュを乗り切るには?**
スコープにフォールバックを宣言(`prefix=writer|fallback`)し、全ノードで `kevy-elect` を走らせます。ライターが `down_after_ms` を超えてハートビートを欠くと、フォールバックがそのプレフィックスの書き込みを受け始めます。クライアントは `-MISDIRECTED writer is <fallback>` を受けて追従します。元ライターが復帰したら手動 rejoin リカバリを実行します。

**なぜゴシップ/Raft は恒久的にスコープ外なのですか?**
全書き込みの下にコンセンサスログを置くコストは、kevy を選ぶ理由になっているスループットとテール遅延の優位を打ち消してしまいます。静的 config + クォーラムハートビート設計はホットパス上で状態機械レプリケーションを払わずにフェイルオーバー分岐を与えてくれます。本当にコンセンサスバックの key-value ストアが必要なワークロードなら、kevy は適さない道具です。
