# レプリケーション

kevyがプライマリから1つ以上のレプリカへ書き込みをストリーミングする仕組み、手動またはクォーラムでフェイルオーバーする方法、そして組み込みプロセスが読み取りレプリカとして同じストリームに参加する方法を説明します。

## このドキュメントが必要になるとき

次のいずれかに当てはまるとき、レプリケーションの出番です。

- **読み出しのファンアウト。** 1台のプライマリがすべての書き込みを受け、1つ以上のレプリカが[`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw)クライアントの後ろでラウンドロビンしながら読み出し負荷を吸収する構成。
- **HAフェイルオーバー。** 現プライマリが落ちたとき、生き残ったレプリカに自動で新プライマリを選出させたい。クォーラムベースの昇格には[`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect)を追加します。計画的なゼロロスの引き継ぎには`FAILOVER` verbを、手動昇格には`REPLICAOF NO ONE`を使ってください。
- **レプリカとしての組み込み。** アプリケーションが[`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded)をプロセス内キー空間として使いつつ、真実の源は`kevy`サーバーに置きたい場合。組み込み側はプライマリをインメモリでミラーし、ネットワーク往復ゼロで読み出しを返します。書き込みはローカルでは拒否されるので、プライマリへ送る必要があります。

`kevy`ノードが1台しかないなら、本書は不要です。クロスDCのアクティブ・アクティブ、ゴシップディスカバリ、オンラインリシャーディング、Raft、AUTH、TLSが必要なら、kevyがそれらを提供することは永久にありません。別のシステムを選んでください。

## 中心となる考え方

プライマリの`kevy`はシャードごとに専用のレプリケーションリスナーを開きます。適用された各変更は、単調増加する64ビットオフセットを伴うRESPエンベロープ（`*2\r\n:<offset>\r\n<argv>`）としてエンコードされ、シャードごとの有界リングバックログにプッシュされます。接続中の各レプリカは、自分が最後にackしたオフセットからストリーミングします。要求されたオフセットがバックログから流れ去っていれば、プライマリはそのシャードのキー空間のスナップショットをインラインで送り、そのまま隙間なくライブストリーミングへ戻ります。レプリカは実行中に`REPLICAOF host port`でターゲットを切り替えられ、`REPLICAOF NO ONE`で自分を降格できます。チェーンレプリケーション（レプリカのレプリカ）はワイヤ上でサポートされず、適用パスで防御的に拒否されます。

v3.14以降、この接続は双方向です。レプリカはフレームを適用しながら、同じレプリケーション接続上に`REPLCONF ACK <offset>`を書き戻します。これによりプライマリは、単なる「送信済み」ではなくレプリカごとの**acked**位置を保持します。`INFO replication`の`slave0`行と`WAIT`バリアが読むのはこの値です。逆方向には、プライマリが1Hzでアウトオブバンドのハートビート`+PING <generation> <next_offset>`を追記します。これはオフセット空間を消費せず、レプリカに自己計測のラグ（`slave_lag_frames`）とリンクの生存性（`master_link_status`）を与えます。`generation`フィールド（v3.16）は途切れのない1つのオフセット履歴を識別し、フェイルオーバーで増加します。v3.16以前の1数値形式`+PING <next_offset>`も引き続きデコードできます。

v3.15以降、トポロジは**対称**です。`role = "replica"`で起動したノードもフルのレプリケーションリスナーとソースをbindするため、昇格したレプリカは即座に下流レプリカにサービスを提供できます。設定の編集も再起動も不要です。

```
                  +-----------------+
   writes ──────► |    primary      |
                  |  shard 0..N-1   |
                  |  port_base + i  |
                  +--------+--------+
                           │ シャードごとの RESP ストリーム (offset, argv)
            ┌──────────────┼──────────────┐
            ▼              ▼              ▼
       +---------+    +---------+    +---------+
       | replica |    | replica |    | embed   |
       |   A     |    |   B     |    | (in-proc|
       |  reads  |    |  reads  |    |  reader)|
       +---------+    +---------+    +---------+
```

同じレプリケーションストリームが3種類のサブスクライバを供給します。レプリカとして動く完全な`kevy`サーバー、レプリカモードで開いた組み込みの`kevy-embedded` `Store`、そして（間接的に）フェイルオーバー判断のため全員の`repl_offset`を見守るクォーラムエレクターです。

## 動かしてみる例

以下の例では、プライマリ1台とレプリカ1台を立ち上げ、レプリカを実行中に再ターゲットし、ロールをプローブし、同じプライマリにプロセス内の組み込みリーダーを取り付けます。

### 1. プライマリの`kevy.toml`

```toml
[replication]
role             = "primary"
listen_port_base = 16004        # shard i binds replication on listen_port_base + i
replication_buffer_size = 268435456   # 256 MiB ring backlog per shard
reconnect_window_ms     = 60000       # how long to hold a slot for a reconnecting replica
```

起動します：

```sh
kevy --config /etc/kevy/primary.toml --port 6004
```

これでプライマリのシャード0は、`:6004`でRESPクライアントトラフィックを、`:16004`でレプリケーション接続を受けるようになります。

### 2. レプリカの`kevy.toml`

```toml
[replication]
role     = "replica"
upstream = "primary.internal:16004"   # プライマリの listen_port_base
```

2台目のホストで起動します：

```sh
kevy --config /etc/kevy/replica.toml --port 6004
```

各ローカルシャードはランナースレッドを開き、`(upstream_host, upstream_port_base + shard_index)`に接続し、`REPLICATE FROM <offset> ID <replica_id>`でハンドシェイクし、`+ACK <offset>`を読み、ローカル再発行を抑止するガードの中でフレームをシャードの適用パスへストリーミングします。

### 3. レプリカを実行中に再ターゲットする

```sh
redis-cli -p 6004 REPLICAOF new-primary.internal 16004
# +OK
```

レプリカはランナー群を止め（ブロックされたreadが抜けられるようソケットをシャットダウンし）、新しいupstreamをパースし、新しいランナーをspawnします。ローカルストアは**ワイプされません**。新プライマリからのフレームは既存データの上に着地します。クリーンなリプレイをしたければ、事前に`FLUSHALL`してください。

### 4. レプリカを手で昇格する

```sh
redis-cli -p 6004 REPLICAOF NO ONE
# +OK
```

すべてのランナースレッドが止まり、有効ロールが`master`に切り替わります。ローカルデータは最後に適用されたフレームの状態のまま残ります。`role = "replica"`で起動したノードはすでにフルのレプリケーションリスナーをbindしている（トポロジ対称性、v3.15）ため、昇格した瞬間から下流レプリカにサービスできます。設定の編集も再起動も不要です。下流リスナーを欠くのは、`standalone`で起動して実行中に再ターゲットされたノードだけです。

協調的でゼロロスの引き継ぎ（書き込みの静止、ターゲットの追いつき待ち、昇格、追従）には、代わりに`FAILOVER` verbを使ってください。下記の*フェイルオーバー*を参照してください。

### 5. ロールをプローブする

```sh
redis-cli -p 6004 ROLE
# 1) "master"
# 2) (integer) 12345678
# 3) 1) 1) "10.0.0.21"
#       2) (integer) 6004
#       3) (integer) 12345670

redis-cli -p 6004 INFO replication          # on the primary
# role:master
# connected_slaves:1
# slave0:ip=10.0.0.21,port=6004,state=online,offset=12345670,sent=12345678,lag=8
# master_repl_offset:12345678

redis-cli -p 6004 INFO replication          # on the replica
# role:slave
# master_host:primary.internal
# master_port:16004
# master_link_status:up
# master_last_io_seconds_ago:0
# slave_read_only:1
# slave_repl_offset:12345670
# slave_lag_frames:0
```

両側ともハートビート/ACKに基づく真値を報告します（v3.14）。プライマリ側では、`slave0`行の`state`はレプリカ最初の`REPLCONF ACK`で`syncing → online`に切り替わり、`offset`はその**acked**位置、`lag`はフレーム単位です。レプリカ側では、直近3秒以内にハートビートが着地していれば`master_link_status`は`up`で、`slave_lag_frames:0`は追いついていることを意味します。フィールドごとの意味は[`docs/availability.md`](../availability.md)の*Observability*節にあります。

`REPLICAOF`によるライブなランタイム状態は、応答内の静的configよりも常に優先されます。さらにelectクォーラムが構成されている場合は、ライブの選挙ロールが両者に優先します。

### 6. レプリカとして組み込む（ワンライナー）

アプリケーションは[`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded)を使って、プロセス内から同じレプリケーションストリームに参加できます。

```rust
use kevy_embedded::Store;

let store = Store::open_replica("primary.internal:16004")?;
assert!(store.is_replica());

// Local writes are rejected with READONLY.
assert!(store.set(b"local", b"nope").is_err());

// Reads pay zero network round-trip — the keyspace lives in this process.
if let Some(v) = store.get(b"hello")? {
    println!("{:?}", v);
}
```

組み込み側は同じ`listen_port_base`のシャードに接続し、届いた順にフレームを適用し、ローカルのarenaから直接読み出しを返します。実行可能なコピーは[`crates/kevy-embedded/examples/replica.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/replica.rs)にあります。

## ノブ

サーバー側TOMLの`[replication]`配下のキー：

| キー | デフォルト | 意味 |
|---|---|---|
| `role` | `"standalone"` | `"standalone"`=サブシステム休眠。`"primary"`はレプリケーションリスナーを開く。`"replica"`は`upstream`から引くランナーをspawnする。 |
| `listen_port_base` | `0`（= クライアントポート + 10000） | シャード`i`は`listen_port_base + i`でレプリケーションをbindする。v3.15以降は**レプリカもこのリスナーをbindする**（昇格対称性）。 |
| `upstream` | 未設定 | レプリカ専用。プライマリのレプリケーションポートベースの`host:port`。各ローカルシャードは`(host, port + shard_index)`を狙う。 |
| `replication_buffer_size` | `268435456`（256 MiB） | バイト単位のシャードごとリングバックログ。この窓内の再接続はライブパスに留まる。それより古いオフセットはスナップショット送出をトリガする。 |
| `reconnect_window_ms` | `60000` | 切断したレプリカのオフセット用スロットを、プライマリが回収するまで予約しておく時間。 |
| `replica_read_only` | `true` | レプリカでのクライアント書き込みを`-READONLY`で拒否する。レプリケーション適用パスと管理系verbはこのゲートをバイパスする。 |
| `replica_max_staleness_ms` | `0`（オフ） | 有界ステイルネス。最後のプライマリハートビートがこの境界より古いレプリカは、読み出しを`-STALE`で拒否する。[`docs/availability.md`](../availability.md)のラダー第3段。 |
| `min_replicas_to_write` | `0`（オフ） | 健全なレプリカ（ACK済みのライブ接続）がN台未満のとき、プライマリは書き込みを`-NOREPLICAS`で拒否する。ラダー第4段。 |
| `min_replicas_max_lag_ms` | `10000` | `min_replicas_to_write`用に予約された鮮度ウィンドウ。 |
| `single_source` | `false` | upstreamがシャードごとのポート群ではなく、1ポート上の1つのストリーム（組み込みライタ）であることを示す。下記*プライマリとして組み込む*を参照。 |

両ロールともレプリケーションポート帯をbindするため、1台のマシンに複数インスタンスを同居させる場合は、クライアントポートを最低`nshards`離してください。そうしないと、デフォルトのレプリケーション帯（`クライアントポート + 10000 … + 10000 + nshards − 1`）が衝突します。

[`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect)を構成する場合、`[cluster]`ブロックがクォーラム用のノブを追加します。

| キー | デフォルト | 意味 |
|---|---|---|
| `node_id` | 未設定 | このノードの安定ID（32BまでのASCII）。選挙のタイブレーカに使う。 |
| `elect_port_base` | `0`（= クライアントポート + 200） | ハートビートと投票用のコントロールプレーンTCPポート。ノードごとに1リスナー。 |
| `peers` | 空 | 自分を含む全クラスタノードの`id@host:elect_port:client_port,…`。空ならエレクターは休眠。 |

拡張された3フィールドのピア構文を使ってください。選挙トラフィックはelectポートを走り、再ターゲットと`-MISDIRECTED`応答はクライアントポートを使います。レガシーの`id@host:port`形式は両者が等しいと仮定しますが、それが望みどおりであることはまずありません。

選挙のタイミングは固定定数であり、configキーではありません。ハートビートは200msごと、ピアは5秒の沈黙でDOWN、候補者はクォーラムのACCEPTを3秒待ちます。（本ページの以前の版はこれらを`[cluster]`キーとして列挙していましたが、configパーサはそれらを拒否します。）

クォーラムは`N/2 + 1`です。N=2では両ノードの生存が必要で（どちらかがDOWNすると生存側は読み取り専用にロックされます）、リンターが警告を出します。フェイルオーバーが必要なデプロイではN ≥ 3を使ってください。

計画に織り込むべき帰結が1つあります。electクォーラムの下では、`[replication] role = "primary"`は初期の*希望*にすぎません。書き込み権威は選挙に勝つことで得られます。クォーラムメンバーは全員読み取り専用でブートし、勝つまで書き込みを保留します（コールドスタートも同様で、最初の書き込みの前に選挙1ラウンド分のコストを払います）。このクランプこそが、再起動した空のプライマリがクラスタを消し去るのを防ぎます。全容は[`docs/availability.md`](../availability.md)の*Election-only write authority*節にあります。

## フェイルオーバー

プライマリロールを移す経路は2つあり、どちらも上記のストリーム機構の上に構築されています。運用上の詳細（手順、タイミング、エラー契約）は[`docs/availability.md`](../availability.md)にあります。

**計画的：`FAILOVER host port [TIMEOUT ms] | ABORT`**（v3.15）。プライマリ上で、ターゲットレプリカの*クライアント*アドレスを指定して実行します。`+OK`と応答した後、バックグラウンドスレッドで引き継ぎます。書き込みを静止させ（`-QUIESCED`）、ターゲットの`INFO replication`を追いつき（`slave_lag_frames:0`）までポーリングし、昇格させ（`REPLICAOF NO ONE`）、その後は自分がレプリカとして追従します。引き継ぎは`クライアントポート + 10000`へ再ターゲットするため、ターゲットはデフォルトの`listen_port_base`で動いている必要があります。タイムアウト（デフォルト10,000ms）すると静止はロールバックされます。

**クラッシュ時：クォーラム選挙**（v3.15）。全ノードに`[cluster]`ブロックがあれば、ピアたちは死んだプライマリを検出し、適用済みレプリケーションオフセットが最大のレプリカを選出します（同点は最小の`node_id`が破ります）。勝者は書き込みを開いて自分のフィードgenerationを増加させ、敗者は自動で再ターゲットします。再合流した旧プライマリのストリームが新プライマリより*先行*している場合（一度もレプリケートされなかった書き込みの分岐サフィックス）、corrupt-closeではなく**置換式**のスナップショット再同期（ロード前に`FLUSHALL`）を受けます。分岐は破棄され、ノードはマジョリティの履歴に収束します。

## トレードオフと限界

レプリケーションは**デフォルトで非同期**です。プライマリは、どのレプリカがフレームを適用したかを知る前にコミットして返信します。レプリカは、フレームがワイヤを渡り、シャードごとのチャネルを抜けて適用パスに入るまでの時間分だけ遅れます。特定の書き込みや読み出しにそれ以上の保証が必要なら、呼び出しごとに購入してください。`WAIT n timeout`はn台以上のレプリカが確認するまでブロックし、`REPL.TOKEN` + `REPL.WAIT`は選んだレプリカ上でread-your-writesを与え、2つのconfigキーが有界ステイルネス（`-STALE`）と最少レプリカ書き込みゲート（`-NOREPLICAS`）を追加します。ラダーの全段は[`docs/availability.md`](../availability.md)にあります。

| 関心事 | 答え |
|---|---|
| 書き込み耐久性 | ローカルストアとバックログリングに着地し次第、プライマリがackする。レプリカは後から追いつく。`WAIT n timeout`はn台以上が確認するまでブロックする（レプリカのackはfsyncではない。availability.mdを参照）。 |
| 読み出し整合性 | レプリカは遅れる可能性がある。`kevy-cluster-rw`経由で`request_read(…, consistent = true)`を送ってプライマリで読むか、レプリカ自体でのread-your-writesには`REPL.TOKEN` + `REPL.WAIT`を使う。 |
| レプリカが遅れすぎた | 再接続のオフセットがリングから流れ去っていれば、プライマリがそのシャードのスナップショットをインラインで送り、スナップショット末尾のオフセットからライブフレームを再開する。隙間なし、オペレータ操作なし。 |
| バックログのサイジング | `replication_buffer_size ≈ peak_writes_per_sec × avg_argv_bytes × reconnect_window_seconds`。大きすぎるのは無害で、小さすぎるとスナップショット送出に落ちる。 |
| 何がフェイルオーバーするか | 新プライマリへの書き込み。`kevy-elect`構成時は自動、それ以外は手動。既存の`kevy-cluster-rw`クライアントは新プライマリを学習し次第、書き込みを再ルーティングする。切り替えの間隙にあったin-flightの書き込みは大きな音を立てて失敗する。 |
| 何がフェイルオーバーしないか | クロスDCトラフィック、ゴシップで発見したピア、オンラインリシャーディング、AUTH/TLS。kevyはこれらをいずれも提供しない。シングルDC専用。 |
| チェーンレプリケーション | ワイヤ上に存在しない。レプリカの適用パスは下流に再発行しない。誤設定は防御的に拒否される。 |
| 分断中のマイノリティ側書き込み | 有界で、その後失われる。厳密な過半数が見えないクォーラムプライマリは、1リースウィンドウ以内に自分の書き込みをフェンスする（`-NOREPLICAS primary lost quorum; writes fenced`）。よってサイレントに吸収される窓は約5秒で、その中のすべての書き込みは大きな音を立てて失敗する。分断されたマイノリティは昇格できず、分断が癒えると降格し、レプリケートされなかった分岐サフィックスは破棄され、スナップショットでマジョリティの履歴に再同期する。 |

ワイヤフォーマット（ライブフレームのエンベロープ、スナップショット送出、ハンドシェイク）は[`crates/kevy-replicate/docs/wire.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/wire.md)と[`crates/kevy-replicate/docs/snapshot.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/snapshot.md)に文書化されています。エレクターのプロトコルは[`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md)にあります。

## FAQ

**レプリカをどう昇格しますか？**
計画的かつゼロロスなら、現プライマリで`FAILOVER host port`を実行します（上記*フェイルオーバー*参照）。手動なら、レプリカに接続して`REPLICAOF NO ONE`を実行します。有効ロールは即時に`master`へ切り替わり、ローカルストアは保たれ、書き込みが受け入れられ、（v3.15以降は）すでにbind済みのレプリケーションリスナーが即座に下流レプリカへのサービスを開始します。自動なら、全ノードで`node_id`、`elect_port_base`、`peers`リストを持つ`[cluster]`を構成します。適用済みオフセットが最大の生存レプリカがクォーラムで勝ちます。

**レプリカがプライマリになり、さらにレプリカに戻れますか？**
はい。`REPLICAOF NO ONE`はデータに触れずupstreamリンクだけを降格します。続けて`REPLICAOF host port`を実行すれば新プライマリへ再アタッチします。ローカルストアは両方の遷移をまたいで保持されます。新しいupstreamからクリーンにリプレイしたければ、事前に`FLUSHALL`してください。

**データロスの窓は？**
「プライマリがクライアントにackする」から「すべてのレプリカがフレームを適用した」までの間隔です。レプリケーションはデフォルトで非同期なので、書き込みをackした直後にプライマリがクラッシュし、どのレプリカもまだそのフレームを持っていなければ、その書き込みは失われます。窓の大きさはワークロード依存で、シングルDCのLANではたいていサブミリ秒です。単一ノードの喪失を生き延びる必要のある書き込みには、`WAIT 1 <timeout>`を続けてください。`WAIT`で確認された書き込みは2ノード上に存在し、クラッシュ選挙は最も進んだレプリカを選ぶため、生き残ります（[`docs/availability.md`](../availability.md)を参照）。それでもレプリカのackはfsyncではありません。電源断をまたぐ耐久性が必要なら、プライマリ側で[`docs/persistence.md`](persistence.md)（AOF + RDB）とレプリケーションを併用してください。

**レプリカから読めますか？**
はい。それこそがレプリカを追加する主な理由です。[`kevy-cluster-rw::ReadWriteClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw)を使えば、書き込みはプライマリへ送られ、読み出しは渡したレプリカシードでラウンドロビンされます。直近の書き込みを必ず観測したい読み出しは、同じクライアントのconsistent-readパスでプライマリ経由に強制してください。

**レプリカが遅れすぎてしまいました。どう復旧しますか？**
何もしないでください。プライマリは、レプリカが要求したオフセットがもうバックログリングにないことを検出すると`TooOld`を返し、同じRESPワイヤ接続でシャードのキー空間スナップショットをインラインで送り、スナップショット末尾のオフセットからライブフレームを再開します。レプリカはスナップショットを差し替え、ライブの末尾を適用し、追いつきます。空から再構築したければ、レプリカを止め、データディレクトリを削除して再起動してください。ランナーは`from_offset = 0`で接続し、キー空間全体がスナップショット送出されます。

## 関連項目

- [`docs/availability.md`](../availability.md) — 運用の半分。トポロジ、整合性ラダー、計画フェイルオーバーとクラッシュフェイルオーバー、エラー契約。本ページが機構（ワイヤ、フレーム、スナップショット送出）を扱い、あちらは何を実行しクライアントに何が見えるかを扱います。
- [`docs/cluster.md`](cluster.md) — マルチシャード公開とスロットルーティングの`ClusterClient`。レプリケーションと直交し、組み合わせ可能。
- [`docs/persistence.md`](persistence.md) — RDBとAOF。スナップショット送出パスはオンディスク形式をワイヤ上で再利用する。
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) — 読み書き分離クライアント。
- [`crates/kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) — クォーラムフェイルオーバー。
- [`crates/kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) — レプリカとして組み込む`Store::open_replica`。

## プライマリとして組み込む（v3.2）

組み込みアプリケーションがプライマリになり、kevyサーバーをそのレプリカにすることもできます。これにより、プロセス内ストアに読み出しスケーリングとフルのクエリ面（レプリカはレプリケートされたデータの上に自前のインデックス/ビュー/集約を宣言できます）が与えられます。

```rust
// the application (primary)
let store = Store::open(
    Config::default().with_shards(4).with_embed_writer("127.0.0.1:7101"),
)?;
```

```toml
# the server replica (replica.toml)
[replication]
role = "replica"
upstream = "127.0.0.1:7101"
single_source = true          # ONE upstream stream, hash-routed locally
```

`single_source = true`はサーバーに、upstreamがサーバー間レプリケーションのシャードごとのポート群ではなく、単一のストリーム（組み込みライタソース）であることを伝えます。ランナーは1つだけ接続し、キー付きフレームはキーのハッシュでローカルシャードへルーティングされ、FLUSHALL/FLUSHDBはブロードキャストされ、スナップショット送出はペイロード全体をブロードキャストして、各シャードは自分のハッシュスライスだけをロードします。

オフセット0（フレッシュ）から、またはバックログウィンドウを過ぎた位置からハンドシェイクするレプリカは、組み込みソースからフルのスナップショット送出を受けます（v1.21のアンチスコープで、v3.2でクローズ）。全シャードのポイントインタイムのフリーズとas-ofオフセット、続いてライブフレームです。

CDCフィード（[docs/cdc.md](../cdc.md)）との関係：両者は設計上共存します。レプリケーションソースはレプリカの整合性（インフラプレーン、ソースごとのオフセット）を担い、フィードはアプリケーションCDC（`(generation, offset)`カーソル、プレフィックスフィルタ、at-least-once）を担います。両者を統合してしまうと、アプリ向けCDCのセマンティクスがレプリカプロトコルに縛られてしまいます。

ゲート：`bench/repligate.sh` — 真の2プロセス構成で、フレッシュなレプリカへのスナップショット送出、静止後のダイジェスト安定性、再起動の再同期、そしてレプリケートされたデータ上でのレプリカローカルな`IDX.CREATE`/`IDX.QUERY`を検証します。
