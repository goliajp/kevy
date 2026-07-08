# クラスタ

kevyのクラスタ機能には独立した2つのレイヤがあります。**シングルノードのマルチシャード公開**（1プロセスで、すべてのシャードがRedis Clusterを話す）と、**マルチノードのレプリケーション+スコープ付きマルチライター**（プライマリ、レプリカ、組み込み、クォーラムフェイルオーバー）です。片方だけ、両方、あるいはどちらも使わない、が選べます。

## 2レイヤの概観

**シングルノードクラスタモード。** 1つのkevyプロセスがキー空間をNシャードに分割し、各シャードを決定論的なシャード別ポート上の仮想クラスタノードとして公開します。`CLUSTER SLOTS / SHARDS / NODES`は本物のCRC16パーティションを報告します。キーを意識するクライアント（`redis-cli -c`、`redis-benchmark --cluster`、市販のクラスタ対応ライブラリ、同梱の[`ClusterClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client/examples/cluster.rs)）は、各キーをハッシュして所有シャードに直接接続します。利得は機械的です。サーバー側のクロスシャードホップが消えれば、それがそのままスループットの向上とテール遅延の低下になります。

**マルチノードクラスタ。** kevyサーバーは**プライマリ**として、1つ以上の**レプリカ**（kevyサーバー、またはプロセス内の[`kevy-embedded`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-embedded)ストア）へ書き込みログをストリーミングできます。プライマリはさらに、**スコープ付き書き込み**をプレフィックス単位で委譲できます。`[cluster] scopes`が、`app:billing:*`の書き込みはどのノード、`app:auth:*`はどのノード、と宣言します。違うノードに着地した書き込みは`-MISDIRECTED writer is <host:port>`を受け取り、クライアントがそれに追従します。[`kevy-elect`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-elect)はクォーラムのコントロールプレーンを提供します。死んだノードにDOWNのフラグを立て、レプリケーショントポロジのために交代プライマリを選出し（v3.15）、スコープの宣言済みフォールバックを昇格させます。オペレータが発行する`MOVE-SCOPE`は、quiesce窓のもとでプレフィックスを移行します。

## このドキュメントが必要になるとき

| 状況 | 使うもの |
|-----------|-----------|
| 1プロセス、キーを意識するクライアント、クロスシャードホップを消したい | シングルノードクラスタモード + `ClusterClient` |
| 1ホスト上で市販のRedis Clusterツールと互換にしたい | シングルノードクラスタモード |
| 別マシンまたはプロセス内からホットな読み出しを返したい | マルチノード：プライマリ+レプリカ（またはレプリカとして組み込み） |
| キープレフィックスで分割された複数のライターを別々のホストで動かしたい | マルチノード：スコープ付きマルチライター |
| 人手を介さずにライターのクラッシュを乗り切りたい | マルチノード：`kevy-elect`クォーラム選挙（プライマリ）/スコープフォールバック（スコープライター） |
| 1プロセス、低負荷、普通のクライアント | どちらも不要。デフォルトのプロキシポートで十分 |

2つのレイヤは合成できます。クラスタモードのプライマリがNシャードを広告し、各レプリカもNシャードで動き、ルーティングクライアントが両者をつなぎます。

---

# レイヤ1 — シングルノードクラスタモード

## 中心となる考え方

普通のkevyプロセスは1つのポートで全コマンドを受け、所有シャードへミスルーティングされたキーを内部で転送します。この転送は正しく動くのですが、ホットパスではp99遅延を支配し、スループットの上限を作ってしまいます。クラスタモードは各シャードを独自のポートで公開します。キーを意識するクライアントはCRC16-XMODEMでキーをハッシュし、`CLUSTER SLOTS`から所有シャードを引き、そこへ直接接続します。転送も`-MOVED`も発生しません。

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

シャード`i`は常に`port_base + 1 + i`にbindします（`port_base`はTOMLで上書きできます）。メインポートは、クラスタを話さないクライアント向けにプロキシとしての挙動を保ちます。シャード別ポートは、間違った所有者にキーが届くと`-MOVED <slot> <host:port>`を返します。

キー空間全体を対象とするコマンド（`KEYS`、`SCAN`、`DBSIZE`、`FLUSHALL`）は、どのポートで実行してもキー空間全体を対象とし続けます。kevyが内部でファンアウトするので、クライアントが面倒を見る必要はありません。

## 有効化

```toml
# kevy.toml
port = 6004

[cluster]
enabled   = true
# port_base = 6004   # 既定は `port`。シャードは port_base + 1 + i に存在。
```

CLI/環境変数での同等指定：

```sh
kevy --port 6004 --threads 8 --cluster      # シャードポート 6005..6012
KEVY_CLUSTER=1 kevy --port 6004 --threads 8
```

データディレクトリをクラスタモードに入れたり出したりすると、起動時に一度だけキーが再ホームされます。元のファイルは`*.premigration.<ts>`としてバックアップされます。

## Rustから`ClusterClient`を使う

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

実行可能なシード例は[`crates/kevy-client/examples/cluster.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client/examples/cluster.rs)に、ベンチマークは[`crates/kevy-client/examples/cluster_bench.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client/examples/cluster_bench.rs)にあります。

### ルーティングがクロスシャードホップを消す仕組み

1. **発見。** `connect`はシードに`CLUSTER SLOTS`を送り、各シャードの`[start, end, host, port]`を読み、16384エントリの`slot → shard-index`テーブルを構築します。テーブルはサーバーが広告するレンジから作られるため、クライアントがパーティショニングの計算を再実装することはありません。
2. **ルーティング。** すべてのシングルキー・コマンドは`key_hash_slot(key)`（`{hashtag}`があればその部分の、なければキー全体のCRC16-XMODEM）を計算し、そのスロット所有者へのコネクションに直接送ります。
3. **必要なところだけファンアウト。** `dbsize`や`flushall`などクラスタ全体のコマンドはサーバー側で処理されます。クライアントは1回呼ぶだけです。

16コアのlx64マシン上、並行数64のGETでクロスシャードホップを消したところ、実測スループットは333k ops/sから533k ops/s（1.6倍）に上がり、p99は3858µsから260µs（約15分の1のテール）に下がりました。`cargo run -p kevy-client --release --example cluster_bench`で再現できます。

> ホップのコストが見えるのは、クリーンなマシンに負荷を掛けたときだけです。小さな同居型クラウドVMでは、差はスケジューリングノイズに埋もれて見えません。

### クロススロットのマルチキー・コマンド

Redis Clusterと違い、kevyはシングルノードクラスタでマルチキー・コマンド（`MGET`、`MSET`、`SUNION`、トランザクション、ブロッキングファンアウト）がシャードをまたいでも`-CROSSSLOT`を返**しません**。サーバーがシャードをまたいで要求を満たします。kevyは単一マシン上でRedis Clusterのスーパーセットです。どのRedis Clusterクライアントもそのまま動き、さらに`-CROSSSLOT`に当たっていたはずの操作も動きます。アトミック性のためにデータを同居させたい場合は`{hashtag}`の共有が依然として正しい道具ですが、正しさのための必須要件ではなくなっています。

### クラスタポートでサポートされる`CLUSTER`コマンド

| コマンド | 挙動 |
|---------|-----------|
| `CLUSTER SLOTS` | 本物のパーティション。シャードごとに`[start, end, host, port]`1行。 |
| `CLUSTER SHARDS` | 同じデータの新しい形式。プライマリノードのみ。 |
| `CLUSTER NODES` | フラットなテキストのマニフェスト。シャードごとに1行、IDはシャードインデックスから派生。 |
| `CLUSTER MYID` | 呼び出しに答えたシャードの決定論的なID。 |
| `CLUSTER KEYSLOT <key>` | `{hashtag}`またはキー全体のCRC16-XMODEM。 |
| `CLUSTER COUNTKEYSINSLOT <slot>` | 所有シャードのインデックスを歩いた生のカウント。 |
| `CLUSTER COUNT-FAILURE-REPORTS <id>` | 常に0。このレイヤに故障検出器はない。 |
| `CLUSTER INFO` | `cluster_enabled:1`、`cluster_state:ok`、スロットカバレッジを返す。 |
| `CLUSTER RESET`、`CLUSTER FORGET`、`CLUSTER MEET`、`CLUSTER FAILOVER`、`MIGRATE`、`ASK` | 未実装。*設計上のスコープ外*節を参照。 |

### 生のルーティングヘルパーへのフォールバック

```rust
// 任意のシングルキー・コマンドを所有シャードへルーティング。
let reply = cc.request_keyed(b"mykey", &[b"STRLEN".to_vec(), b"mykey".to_vec()])?;
// キーなしコマンドは任意のシャードへ。
let reply = cc.request_unkeyed(&[b"PING".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

`ClusterClient`は、文字列、ハッシュ、リスト、セット、ソート済みセット、pub/sub、マルチキーの`DEL`/`EXISTS`の共通verbをラップします。pub/subはプロセス全体に届きます。どのポートの`Subscriber`も、どのシャードが`PUBLISH`を受けたかに関係なく、発行されたすべてのメッセージを見ます。

---

# レイヤ2 — マルチノードクラスタ

## プライマリとレプリカ

kevyサーバーは、自分の書き込みログをストリーミングするプライマリにも、それをミラーするレプリカにもなれます（デフォルトのロールは`standalone`で、レプリケーションは休眠しています）。プライマリはシャードごとに専用のレプリケーションリスナーをbindします。レプリカは接続し、最後に適用したオフセットを渡し、ストリーミングされてくるフレームをローカルシャードに適用します。チェーンレプリケーション（レプリカのレプリカ）はサポートされません。

```toml
# primary.toml
port = 6004

[replication]
role             = "primary"
listen_port_base = 16004    # 任意。デフォルトは port + 10000
```

```toml
# replica.toml
port = 6004

[replication]
role     = "replica"
upstream = "primary.local:16004"
```

サーバー側の完全なセマンティクス（バックログのサイジング、スナップショットの取り込み、ハートビートとACK）は[`docs/replication.md`](replication.md)に、フェイルオーバー（計画的な`FAILOVER`とクラッシュ選挙）と整合性ラダーは[`docs/availability.md`](../availability.md)にあります。本書にとって重要な事実は、同じワイヤプロトコルがクラスタモードのレプリケーションも運ぶことです。`[cluster] enabled = true`で動くプライマリはNシャード分の書き込みをストリーミングし、同じシャード数で動くレプリカがシャード対シャードで適用します。

## レプリカとして組み込む

[`kevy-embedded`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-embedded)のストアはプライマリに直接subscribeして、ネットワークホップなしのプロセス内読み出しを返せます。書き込みは`READONLY`でローカルに拒否されます。

```rust
use kevy_embedded::Store;

// インメモリレプリカ、AOF off、デフォルト再接続(100 ms → 5 s)。
let replica = Store::open_replica("primary.local:16004")?;

let v = replica.get(b"hello")?;
assert!(replica.set(b"k", b"v").is_err());      // READONLY
# Ok::<(), std::io::Error>(())
```

チューニングする場合：

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

ハンドシェイクは`REPLICATE FROM <last-applied-offset> ID <replica_id>`を送ります。プライマリはオフセットをackし、フレームをストリーミングします。最後の`Store`クローンがdropされるとランナースレッドはjoinされ、プライマリはクリーンなFINを観測してスロットを解放します。組み込み側での`PUBLISH`はローカルに許可されます（pub/subはプロセスローカルです）が、キー空間自体は読み取り専用のままです。

## スコープ付きマルチライター

スコープ付きマルチライターは、キープレフィックスごとに書き込みをノードに振り分けます。各ノードは静的なconfigから所有テーブル全体を知っています。非所有者に着地した書き込みは`-MISDIRECTED writer is <host:port>`を返し、クライアントは正しいノードに再試行します。

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

`peers`は`<node_id>@<host>:<port>`エントリのフラットな文字列です。ネスト構造がないので、テンプレート化が容易です。`scopes`は`prefix=writer[|fallback]`のカンマ区切りとしてパースされます。スコープを所有しないノードは単に書き込みを転送し、スコープを所有するノードは自分のスコープの書き込みを受け、それ以外を拒否します。

読み出しはスコープの所有とは独立です。データを持つどのノード（典型的には読み取りレプリカ）でも返せます。スコープの仕組みは、書き込みの帰属だけのためにあります。

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

組み込み側は、`with_embed_writer`に渡したアドレスでレプリケーションリスナーを公開します。ほかのノードは、サーバープライマリから引くのとまったく同じ方法でそこからログを引きます。

## `kevy-elect`クォーラムフェイルオーバー

`kevy-elect`は、`[cluster] node_id`と`peers`の両方が設定されたときにクラスタの全メンバーが走らせる、プロセス内のクォーラムコントロールプレーンです。各ノードは`elect_port_base`（デフォルト：クライアントポート+200）に1本のTCPコントロールリスナーをbindし、全ピアにハートビートを送ります。`down_after`（5秒）を超えて沈黙したピアにはDOWNのフラグが立ちます。メンバーシップは**静的**（オペレータが宣言した`peers`テーブル）で、ロールは**動的**です。選挙はそのテーブルの内側でプライマリを動かします。選挙のタイミング（`hb_interval` 200ms、`down_after` 5秒、`election_timeout` 3秒）は本リリースでは固定定数であり、configキーではありません。

これが2つのフェイルオーバー面を駆動します。

**レプリケーションプライマリの選挙（v3.15）。** 現プライマリがDOWNのとき、資格のあるレプリカ（生存ピアの中で適用済みレプリケーションオフセットが最大のもの。同点は最小の`node_id`が破る）が立候補し、`election_timeout`以内にクォーラム（`N/2 + 1`）のACCEPTを集める必要があります。エポックと投票は、どの応答もノードを出る*前に*`<data_dir>/elect.meta`へ永続化されるため、クラッシュ再起動しても二重投票はあり得ません。勝者は書き込みを開き、敗者は自動でレプリケーションのupstreamを再ターゲットします。これを3つのクランプが支えます。既知のプライマリがない**コールドスタート**は、`down_after`の猶予窓を1回待ってから選挙します。**再起動**したクォーラムメンバーは、選挙が落ち着くまで書き込みを保留します（configの`role = "primary"`は希望にすぎません）。厳密な過半数が見えないプライマリは、1リースウィンドウ以内に**自分の書き込みをフェンス**します（`-NOREPLICAS primary lost quorum; writes fenced`）。運用のウォークスルーは[`docs/availability.md`](../availability.md)にあります。

**スコープフォールバック。** スコープの宣言済みフォールバックは、書き込みを受けるたびにDOWN集合を参照します。ライターがDOWNなら、フォールバックは自分をアクティブな所有者として扱って書き込みを受領し、以降ほかのノードへの書き込みはフォールバックへMISDIRECTされます。元ライターのハートビートが戻るとDOWN集合を抜け、次回の判断でフォールバックは暗黙に退きます。

| ノブ | 意味 | デフォルト |
|------|---------|---------|
| `node_id` | このノードの安定識別子（32BまでのASCII。スコープ所有者と選挙が参照する） | 必須 |
| `peers` | 全クラスタメンバーの`<node_id>@<host>:<elect_port>:<client_port>`リスト | 必須 |
| `elect_port_base` | 選挙コントロールプレーンがbindするTCPポート（ノードごとに1リスナー） | `0` = クライアントポート + 200 |

### 手動でのrejoinリカバリ

元ライターのDOWN期間が長く、その間フォールバックが書き込みを受領していた場合、それらの書き込みはフォールバック上にしかありません。そのスコープの元ライターを再有効化する前に、ライターを止め、フォールバックのデータディレクトリをライター側へコピーし、再起動してください。これでコンセンサスなしの契約を守れます。シャドウ書き込みも二重受領も起こりません。

---

# `MOVE-SCOPE`

`MOVE-SCOPE`は、有界なquiesce窓のもとで、あるライターから別のライターへプレフィックスを移行します。オペレータが発行し、現ライター上で走ります。

```
MOVE-SCOPE <prefix> from <from-node-id> to <to-node-id>
```

手順は次のとおりです：

1. 現ライターは`<prefix>`のローカル状態をMIGRATINGに切り替えます。以降、そのプレフィックス配下のキーへの書き込みは`-QUIESCED migrating to <to-host:port>`を返します。クライアントは少し待って再試行します。
2. ライターはプレフィックスのキー空間スライスをシリアライズし、`MOVE-SCOPE-INGEST <prefix> <bulk>`でターゲットのデータポートへ送ります。
3. ターゲットから`+OK`を受け取ると、ライターは移行をローカルにコミットします。以降、ソース側でのそのプレフィックスへの書き込みは`-MISDIRECTED writer is <to-host:port>`を返します。
4. ほかのクラスタメンバーは、オペレータが新しいconfigをpushして再起動するまで、静的な`scopes`どおりにルーティングを続けます。

移動中に見える2つのワイヤ応答：

| 応答 | 意味 |
|-------|---------|
| `-MISDIRECTED writer is <host:port>` | 書き込みが非所有者に着地した。指定されたホストに再試行する。 |
| `-QUIESCED migrating to <host:port>` | MOVE-SCOPE窓中の一時的なもの。少し待って再試行する。 |

クラスタ対応クライアントは`-MISDIRECTED`でキーごとのターゲットをキャッシュして透過的に再試行します。`-QUIESCED`では数百ミリ秒ほど眠ってから再試行するのが適切です。

移動を途中で中断するとソースライターに復元され、ターゲットに部分適用の状態が残ることはありません。

---

# 設定リファレンス

## シングルノードクラスタモード

| TOML | CLI | 環境変数 | デフォルト | 意味 |
|------|-----|-----|---------|---------|
| `[cluster] enabled` | `--cluster` | `KEVY_CLUSTER=1` | `false` | 各シャードをシャード別ポートで公開する。 |
| `[cluster] port_base` | `--cluster-port-base` | `KEVY_CLUSTER_PORT_BASE` | `port`の値 | シャード`i`は`port_base + 1 + i`をbindする。 |

## レプリケーション

TOMLのみです。レプリケーションのCLIフラグや環境変数はありません：

| TOML | デフォルト | 意味 |
|------|---------|---------|
| `[replication] role` | `"standalone"` | `"primary"`はレプリカへストリーミング。`"replica"`は`upstream`から引く。`"standalone"`=サブシステム休眠。 |
| `[replication] listen_port_base` | `0`（= `port` + 10000） | シャード`i`はbase + `i`でレプリケーションをbindする。レプリカもbindする（v3.15の昇格対称性）。 |
| `[replication] upstream` | 未設定 | レプリカ専用。プライマリのレプリケーションポートベースの`host:port`。 |

完全なキー一覧（バックログのサイジング、`replica_read_only`、`replica_max_staleness_ms`、`min_replicas_to_write`、`single_source`）は[`docs/replication.md`](replication.md)にあります。

## スコープ付きマルチライター + elect

| TOML | 意味 |
|------|---------|
| `[cluster] node_id` | このノードの安定識別子（32BまでのASCII）。 |
| `[cluster] peers` | 全クラスタメンバーの`<node_id>@<host>:<elect_port>:<client_port>`リスト（レガシーの2フィールド形式では両ポートが等しいとみなす）。 |
| `[cluster] scopes` | `prefix=writer[\|fallback]`エントリ、カンマ区切り。 |
| `[cluster] elect_port_base` | 選挙コントロールプレーンがbindするTCPポート。`0`（デフォルト）= `port` + 200。 |

選挙のタイミング（ハートビート200ms、5秒の沈黙でDOWN、選挙タイムアウト3秒）は固定定数であり、configキーではありません。

---

# トレードオフと限界

- **シングルノードクラスタモードは1プロセスです。** 買えるのはクライアント側のキー・ルーティングであって、ホストレベルの耐障害性ではありません。そちらが必要ならレプリカを追加してください。
- **プロキシポートは生き続けます。** クラスタを話さないクライアントは引き続き正しく動作します。ただしクロスシャードホップ付きです。
- **メンバーシップは静的、ロールは動的です。** `peers`と`scopes`は起動時にconfigから読まれます。メンバーシップの変更は「新しいconfigをpushして再起動」であり、設計上ゴシップはありません。その静的テーブルの内側で、選挙が実行中にプライマリロールを動かします（v3.15）。故障ノードが自動で置き換えられることはありません。
- **`MOVE-SCOPE`はプレフィックスの書き込みをquiesceします。** 窓はスライス送出時間で有界です。LAN経由のGBクラスのスコープなら1桁秒です。それより大幅に大きいプレフィックスは、メンテナンス窓に合わせて実施してください。
- **スコープライターとしての組み込みはサービス形状のワークロード**（請求サービス、認証サービスなど）を想定しており、マルチTBのデータセットは想定していません。
- **フォールバック受領後は手動のrejoinリカバリが必要です。** 再有効化の前にフォールバックのデータディレクトリをライターへコピーしてください。自動のコンセンサスキャッチアップはありません。

---

# 設計上のスコープ外

- AUTHとTLS — デプロイのエッジ（サイドカー、メッシュ、LB）で扱うものであり、kevyでは扱いません。
- マルチDCのアクティブ・アクティブとCRDT。
- Raft、Paxos、その他キー空間の下に置くコンセンサスログ。
- ゴシップベースの発見 — `peers`は静的です。
- オンラインリシャーディング、`MIGRATE`、`ASK`リダイレクト。
- 所有が重なるマルチマスター — 各プレフィックスのライターは常にちょうど1つです。

これらが追加されることはありません。シンプルさこそが機能です。

---

# FAQ

**レプリケーションを使うのにクラスタモードは必要ですか？**
いいえ。シングルノードクラスタモードと、レプリケーション/マルチノードのレイヤは独立です。非クラスタのプライマリは非クラスタのレプリカを持てますし、クラスタのプライマリはクラスタのレプリカを持てます。合成は可能ですが、どちらかがもう一方を必須とすることはありません。

**クラスタモードのkevyに対して標準のクラスタ対応クライアント（Lettuce、ioredis、redis-py-cluster）は使えますか？**
はい。`CLUSTER SLOTS / SHARDS / NODES`は本物のパーティションを広告し、間違ったシャードに当たれば`-MOVED`が出ます。これらのライブラリが依存している面はまさにそれだけです。クライアントのルーティングがシャードに届くよう、メインのプロキシポートではなくシャード別ポートに向けてください。

**シングルノードクラスタモードでシャードをまたぐマルチキー・コマンドはどうなりますか？**
成功します。kevyはクロススロットの`MGET`、`MSET`、`SUNION`、トランザクション、ブロッキングファンアウトを、`-CROSSSLOT`を返さずにサーバー側で実行します。アトミック性が重要なケースでは`{hashtag}`による同居は依然として有用ですが、もはや正しさのための要件ではありません。

**オペレータなしでライターのクラッシュを乗り切るには？**
レプリケーションプライマリの場合：全ノードに`[cluster]`ブロック（`node_id`、`elect_port_base`、`peers`）を構成します。死んだプライマリは`down_after`（5秒）後に検出され、最も進んだレプリカがその座に選出されます。再合流した旧プライマリは自動で降格して再同期します（[`docs/availability.md`](../availability.md)を参照）。スコープライターの場合：フォールバックを宣言します（`prefix=writer|fallback`）。ライターのハートビートが`down_after`を超えて途切れると、フォールバックがそのプレフィックスの書き込みを受け始めます。クライアントは`-MISDIRECTED writer is <fallback>`を受けて追従し、元ライターが復帰したら手動のrejoinリカバリを実行します。

**なぜゴシップ/Raftは恒久的にスコープ外なのですか？**
全書き込みの下にコンセンサスログを置くコストは、kevyを選ぶ理由になっているスループットとテール遅延の優位を打ち消してしまいます。静的config+クォーラムハートビートの設計なら、ホットパス上でステートマシンレプリケーションの代金を払うことなく、フェイルオーバーの分岐が手に入ります。本当にコンセンサスバックのkey-valueストアが必要なワークロードには、kevyは適さない道具です。
