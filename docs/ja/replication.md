# レプリケーション

kevy がプライマリから 1 つ以上のレプリカに書き込みをストリーミングする仕組み、手動またはクォーラムによるフェイルオーバーの方法、そして組み込みプロセスが読み取りレプリカと同じストリームに subscribe する方法について説明します。

## このドキュメントが必要になるとき

次のいずれかに当てはまるときにレプリケーションを使います。

- **読み出しのファンアウト。** 1 台のプライマリがすべての書き込みを受け、1 つ以上のレプリカが読み出し負荷を吸収して [`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) クライアントの後ろでラウンドロビン。
- **HA フェイルオーバー。** 現プライマリが落ちたとき、生き残ったレプリカが自動で新プライマリを選出してほしい。クォーラムベースの昇格には [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) を足します。計画的なゼロロス引き継ぎには `FAILOVER` verb を、手動昇格には `REPLICAOF NO ONE` を使ってください。
- **レプリカとして組み込む。** アプリケーションが [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) をプロセス内キー空間として使い、真実の源は `kevy` サーバーに置きたい。組み込みはプライマリをインメモリでミラーし、ネットワーク往復ゼロで読み出しを返します。書き込みはローカルでは拒否され、プライマリへ送る必要があります。

`kevy` ノードが 1 台しかないなら本書は不要です。クロス DC アクティブ・アクティブ、ゴシップディスカバリ、オンラインリシャーディング、Raft、AUTH、TLS が必要なら、kevy はそれらを永久に提供しません — 別のシステムを選んでください。

## 中心となる考え方

プライマリの `kevy` はシャードごとに専用のレプリケーションリスナーを開きます。適用された各変更は RESP エンベロープ(`*2\r\n:<offset>\r\n<argv>`)として、単調増加する 64 ビットのオフセットを伴ってエンコードされ、シャードごとの有界リングバックログにプッシュされます。各レプリカは最後に ack したオフセットからストリーミングします。要求オフセットがバックログから流れ去っていれば、プライマリはそのシャードのキー空間のスナップショットをインラインで送り、そのまま隙間なくライブストリーミングへ戻ります。レプリカはランタイムに `REPLICAOF host port` でターゲットを切り替えられ、`REPLICAOF NO ONE` で自身を降格できます。チェーンレプリケーション(レプリカのレプリカ)はワイヤ上サポートされず、適用パスで防御的に拒否されます。

v3.14 以降、この接続は双方向です。レプリカはフレームを適用しながら、同じレプリケーション接続上に `REPLCONF ACK <offset>` を書き戻します。これによりプライマリはレプリカごとの **acked** 位置(単なる「送信済み」ではなく)を保持します — `INFO replication` の `slave0` 行と `WAIT` バリアが読むのはこれです。逆方向には、プライマリが 1 Hz でアウトオブバンドのハートビート `+PING <generation> <next_offset>` を追記します — オフセット空間を消費せず、レプリカに自己計測のラグ(`slave_lag_frames`)とリンク生存性(`master_link_status`)を与えます。`generation` フィールド(v3.16)は途切れのないひとつのオフセット履歴を識別し、フェイルオーバーで増加します。v3.16 以前の 1 数値形式 `+PING <next_offset>` も引き続きデコードできます。

v3.15 以降、トポロジは**対称**です。`role = "replica"` で起動したノードもフルのレプリケーションリスナー + ソースを bind するため、昇格したレプリカは即座に下流レプリカにサービスできます — 設定編集も再起動も不要です。

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

同じレプリケーションストリームが 3 種類のサブスクライバを供給します。レプリカとして動く完全な `kevy` サーバー、レプリカモードで開いた組み込みの `kevy-embedded` `Store`、そして(間接的に)フェイルオーバー判断のため全員の `repl_offset` を見守るクォーラム選出者です。

## 動かしてみる例

以下の例ではプライマリ 1、レプリカ 1 を立ち上げ、レプリカをランタイムに再ターゲットし、ロールをプローブし、同じプライマリにプロセス内の組み込みリーダーを取り付けます。

### 1. プライマリ `kevy.toml`

```toml
[replication]
role             = "primary"
listen_port_base = 16004        # shard i binds replication on listen_port_base + i
replication_buffer_size = 268435456   # 256 MiB ring backlog per shard
reconnect_window_ms     = 60000       # how long to hold a slot for a reconnecting replica
```

起動:

```sh
kevy --config /etc/kevy/primary.toml --port 6004
```

プライマリのシャード 0 は `:6004` で RESP クライアントトラフィックを受け、`:16004` でレプリケーション接続を受けるようになります。

### 2. レプリカ `kevy.toml`

```toml
[replication]
role     = "replica"
upstream = "primary.internal:16004"   # プライマリの listen_port_base
```

2 台目のホストで起動:

```sh
kevy --config /etc/kevy/replica.toml --port 6004
```

各ローカルシャードはランナースレッドを開き、`(upstream_host, upstream_port_base + shard_index)` に接続し、`REPLICATE FROM <offset> ID <replica_id>` でハンドシェイクし、`+ACK <offset>` を読み、ローカル再発行を抑止するガード内でフレームをシャードの適用パスへストリーミングします。

### 3. ランタイムにレプリカを再ターゲットする

```sh
redis-cli -p 6004 REPLICAOF new-primary.internal 16004
# +OK
```

レプリカはランナー群を止め(ブロックされた read が抜けるようにソケットをシャットダウン)、新しい upstream をパースし、新しいランナーを spawn します。ローカルストアは**ワイプされません** — 新プライマリからのフレームが既存データの上に着地します。クリーンなリプレイをしたければ事前に `FLUSHALL` してください。

### 4. レプリカを手で昇格する

```sh
redis-cli -p 6004 REPLICAOF NO ONE
# +OK
```

すべてのランナースレッドが止まり、有効ロールが `master` に flip します。ローカルデータは最後に適用されたフレームのまま残ります。`role = "replica"` で起動したノードはすでにフルのレプリケーションリスナーを bind している(トポロジ対称性、v3.15)ため、昇格した瞬間から下流レプリカにサービスできます — 設定編集も再起動も不要です。下流リスナーを欠くのは、`standalone` で起動してランタイムに再ターゲットされたノードだけです。

協調的でゼロロスの引き継ぎ(書き込みの静止、ターゲットの追いつき待ち、昇格、追従)には、代わりに `FAILOVER` verb を使ってください — 下記の*計画フェイルオーバー*を参照。

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

両側ともハートビート/ACK の真値を報告します(v3.14)。プライマリ側では、`slave0` 行の `state` はレプリカ最初の `REPLCONF ACK` で `syncing → online` に flip し、`offset` はその **acked** 位置、`lag` はフレーム単位です。レプリカ側では、直近 3 秒以内にハートビートが着地していれば `master_link_status` は `up` で、`slave_lag_frames:0` は追いついたことを意味します。フィールドごとの意味は [`docs/availability.md`](../availability.md) の *Observability* 節にあります。

`REPLICAOF` のライブランタイム状態は、応答中の静的 config よりも常に優先されます — さらに elect クォーラムが構成されている場合は、ライブの選挙ロールが両者に優先します。

### 6. レプリカとして組み込む(ワンライナー)

アプリケーションは [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) を使ってプロセス内で同じレプリケーションストリームに参加できます。

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

組み込みは同じ `listen_port_base` のシャードに接続し、到着順にフレームを適用し、ローカル arena から直接読み出しを返します。実行可能なコピーは [`crates/kevy-embedded/examples/replica.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/replica.rs) にあります。

## ノブ

サーバー側 TOML、`[replication]` 配下のキー:

| キー | デフォルト | 意味 |
|---|---|---|
| `role` | `"standalone"` | `"standalone"` = サブシステム休眠。`"primary"` はレプリケーションリスナーを開く。`"replica"` は `upstream` から引くランナーを spawn。 |
| `listen_port_base` | `0`(= クライアントポート + 10000) | シャード `i` は `listen_port_base + i` でレプリケーションを bind。v3.15 以降は**レプリカもこのリスナーを bind**(昇格対称性)。 |
| `upstream` | 未設定 | レプリカ専用。プライマリのレプリケーションポートベースの `host:port`。各ローカルシャードは `(host, port + shard_index)` を狙う。 |
| `replication_buffer_size` | `268435456`(256 MiB) | バイト単位のシャードごとリングバックログ。この窓内の再接続はライブパスに留まる。古いオフセットはスナップショット送出をトリガ。 |
| `reconnect_window_ms` | `60000` | プライマリが切断レプリカのオフセット用スロットを回収するまでに予約しておく時間。 |
| `replica_read_only` | `true` | レプリカでのクライアント書き込みを `-READONLY` で拒否。レプリケーション適用パスと管理系 verb はこのゲートをバイパスする。 |
| `replica_max_staleness_ms` | `0`(オフ) | 有界ステイルネス: 最後のプライマリハートビートがこの境界より古いレプリカは、読み出しを `-STALE` で拒否する。[`docs/availability.md`](../availability.md) のラダー第 3 段。 |
| `min_replicas_to_write` | `0`(オフ) | 健全なレプリカ(ACK 済みのライブ接続)が N 未満のとき、プライマリは書き込みを `-NOREPLICAS` で拒否する。ラダー第 4 段。 |
| `min_replicas_max_lag_ms` | `10000` | `min_replicas_to_write` 用に予約された鮮度ウィンドウ。 |
| `single_source` | `false` | upstream がシャードごとのポート群ではなく、1 ポート上のひとつのストリーム(組み込みライタ)である — 下記*プライマリとして組み込む*を参照。 |

両ロールともレプリケーションポート帯を bind するため、1 台のマシンに複数インスタンスを同居させる場合はクライアントポートを最低 `nshards` 離してください — さもないとデフォルトのレプリケーション帯(`クライアントポート + 10000 … + 10000 + nshards − 1`)が衝突します。

[`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) を構成する場合、`[cluster]` ブロックがクォーラムノブを足します。

| キー | デフォルト | 意味 |
|---|---|---|
| `node_id` | 未設定 | このノードの安定 ID(≤ 32 B ASCII)。選挙のタイブレーカに使用。 |
| `elect_port_base` | `0`(= クライアントポート + 200) | ハートビートと投票用のコントロールプレーン TCP ポート — ノードごとに 1 リスナー。 |
| `peers` | 空 | 自分を含む全クラスタノードの `id@host:elect_port:client_port,…`。空ならエレクターは休眠。 |

拡張された 3 フィールドのピア構文を使ってください。選挙トラフィックは elect ポートを走り、再ターゲットと `-MISDIRECTED` 応答はクライアントポートを使います。レガシーの `id@host:port` 形式は両者が等しいと仮定しますが、それが望みどおりであることはほぼありません。

選挙タイミングは固定定数であり、config キーではありません。ハートビートは 200 ms ごと、ピアは 5 秒の沈黙で DOWN、候補者はクォーラム ACCEPT を 3 秒待ちます。(本ページの以前の版はこれらを `[cluster]` キーとして列挙していましたが、config パーサはそれらを拒否します。)

クォーラムは `N/2 + 1` です。N=2 では両ノード生存が必要(どちらかが DOWN だと生存側は読み取り専用にロック)。リンターは警告を出します。フェイルオーバーが必要なデプロイでは N ≥ 3 を使ってください。

計画に織り込むべき帰結がひとつ: elect クォーラムでは `[replication] role = "primary"` は初期の*希望*にすぎません。書き込み権威は選挙に勝つことで得られます — クォーラムメンバーは全員読み取り専用でブートし、勝つまで書き込みを保留します(コールドスタートも同様で、最初の書き込みの前に選挙 1 ラウンドを支払います)。このクランプこそが、再起動した空のプライマリがクラスタを消し去るのを防ぎます。全容は [`docs/availability.md`](../availability.md) の *Election-only write authority* 節にあります。

## フェイルオーバー

プライマリロールを移す経路は 2 つで、どちらも上記のストリーム機構の上に構築されています。運用上の詳細(手順、タイミング、エラー契約)は [`docs/availability.md`](../availability.md) にあります。

**計画: `FAILOVER host port [TIMEOUT ms] | ABORT`**(v3.15)。プライマリ上で、ターゲットレプリカの*クライアント*アドレスを指定して実行します。`+OK` と応答し、バックグラウンドスレッドで引き継ぎます。書き込みを静止(`-QUIESCED`)し、ターゲットの `INFO replication` を追いつき(`slave_lag_frames:0`)までポーリングし、昇格(`REPLICAOF NO ONE`)させ、その後レプリカとして追従します。引き継ぎは `クライアントポート + 10000` へ再ターゲットするため、ターゲットはデフォルトの `listen_port_base` で動いている必要があります。タイムアウト(デフォルト 10 000 ms)は静止をロールバックします。

**クラッシュ: クォーラム選挙**(v3.15)。全ノードに `[cluster]` ブロックがあれば、ピアたちは死んだプライマリを検出し、適用済みレプリケーションオフセットが最大のレプリカを選出します(タイは最小の `node_id` が破る)。勝者は書き込みを開いて自分のフィード generation を増加させ、敗者は自動で再ターゲットします。再合流した旧プライマリのストリームが新プライマリより*先行*している場合(一度もレプリケートされなかった書き込みの分岐サフィックス)、corrupt-close ではなく**置換式**スナップショット再同期 — ロード前に `FLUSHALL` — を受けます。分岐は破棄され、ノードはマジョリティの履歴に収束します。

## トレードオフと限界

レプリケーションは**デフォルトで非同期**です。プライマリはどのレプリカがフレームを適用したか知る前にコミットして返信します。レプリカは、フレームがワイヤを渡ってシャードごとのチャネルを抜けて適用パスに入るまでの時間ぶん遅れます。特定の書き込みや読み出しにそれ以上が必要なら、呼び出しごとに購入してください。`WAIT n timeout` は n 個以上のレプリカが確認するまでブロックし、`REPL.TOKEN` + `REPL.WAIT` は選んだレプリカ上で read-your-writes を与え、2 つの config キーが有界ステイルネス(`-STALE`)と最少レプリカ書き込みゲート(`-NOREPLICAS`)を足します。ラダーの全段は [`docs/availability.md`](../availability.md) にあります。

| 関心事 | 答え |
|---|---|
| 書き込み耐久性 | ローカルストアとバックログリングに着地次第プライマリが ack。レプリカは後で追いつく。`WAIT n timeout` は n 個以上が確認するまでブロック(レプリカの ack は fsync ではない — availability.md を参照)。 |
| 読み出し整合性 | レプリカは遅れる可能性がある。`kevy-cluster-rw` 経由で `request_read(…, consistent = true)` を送ってプライマリで読むか、レプリカ自体での read-your-writes には `REPL.TOKEN` + `REPL.WAIT` を使う。 |
| レプリカが遅れすぎる | 再接続のオフセットがリングから流れ去っていれば、プライマリがそのシャードのスナップショットをインラインで送り、スナップショット末尾オフセットでライブフレームを再開 — 隙間なし、オペレータ操作なし。 |
| バックログのサイジング | `replication_buffer_size ≈ peak_writes_per_sec × avg_argv_bytes × reconnect_window_seconds`。大きすぎるのは無害、小さすぎるとスナップショット送出に落ちる。 |
| 何がフェイルオーバーするか | 新プライマリへの書き込み。`kevy-elect` 構成時は自動、それ以外は手動。既存の `kevy-cluster-rw` クライアントは新プライマリを学習次第書き込みを再ルーティング。間隙中の in-flight 書き込みは大きく失敗する。 |
| 何がフェイルオーバーしないか | クロス DC トラフィック、ゴシップで発見したピア、オンラインリシャーディング、AUTH/TLS — kevy はそれらをいずれも提供しない。シングル DC のみ。 |
| チェーンレプリケーション | ワイヤ上なし。レプリカの適用パスは下流に再発行しない。誤設定は防御的に拒否される。 |
| 分断中のマイノリティ側書き込み | 有界で、その後失われる。厳密な過半数が見えないクォーラムプライマリは、1 リースウィンドウ以内に自分の書き込みをフェンスします(`-NOREPLICAS primary lost quorum; writes fenced`)。よってサイレント吸収ウィンドウは約 5 秒で、その中のすべての書き込みは大きく失敗します。分断したマイノリティは昇格できず、分断が癒えると降格し、レプリケートされなかった分岐サフィックスは破棄され、スナップショットでマジョリティの履歴に再同期します。 |

ワイヤフォーマット(ライブフレームエンベロープ、スナップショット送出、ハンドシェイク)は [`crates/kevy-replicate/docs/wire.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/wire.md) と [`crates/kevy-replicate/docs/snapshot.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/snapshot.md) に文書化されています。エレクターのプロトコルは [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md) にあります。

## FAQ

**レプリカをどう昇格しますか?**
計画的かつゼロロス: 現プライマリで `FAILOVER host port` を実行(上記*フェイルオーバー*参照)。手動: レプリカに繋いで `REPLICAOF NO ONE` — 有効ロールは即時に `master` に flip し、ローカルストアは保たれ、書き込みが受け入れられ、(v3.15 以降)すでに bind 済みのレプリケーションリスナーが即座に下流レプリカへのサービスを開始します。自動: 全ノードで `node_id`、`elect_port_base`、`peers` リストの `[cluster]` を構成。適用済みオフセット最大の生存レプリカがクォーラムで勝ちます。

**レプリカがプライマリになり、さらにレプリカに戻れますか?**
はい。`REPLICAOF NO ONE` はデータに触れず upstream リンクだけ降格します。続く `REPLICAOF host port` で新プライマリへ再アタッチ。両方の遷移をまたいでローカルストアは保持されます。新 upstream からクリーンリプレイしたければ事前に `FLUSHALL` してください。

**データロス窓は?**
「プライマリがクライアントに ack する」から「すべてのレプリカがフレームを適用した」までの間隔です。レプリケーションはデフォルトで非同期なので、書き込みを ack した直後にプライマリがクラッシュし、どのレプリカもまだフレームを持っていなければ、その書き込みは失われます。窓のサイジングはワークロード依存 — シングル DC LAN ではたいていサブミリ秒です。単一ノード喪失を生き延びる必要のある書き込みには `WAIT 1 <timeout>` を続けてください。`WAIT` で確認された書き込みは 2 ノード上に存在し、クラッシュ選挙は最も進んだレプリカを選ぶため、生き残ります([`docs/availability.md`](../availability.md) を参照)。レプリカの ack は依然として fsync ではありません。電源断をまたぐ耐久性が必要なら、プライマリ側で [`docs/persistence.md`](persistence.md)(AOF + RDB)とレプリケーションを併用してください。

**レプリカから読めますか?**
はい — それがレプリカを足す主な理由です。[`kevy-cluster-rw::ReadWriteClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) を使い、書き込みはプライマリへ、読み出しは渡したレプリカシードでラウンドロビンします。直近書き込みを必ず観測したい読み出しは、同じクライアントの consistent-read パスでプライマリ経由に強制します。

**レプリカが遅れすぎてしまいました — どう復旧しますか?**
何もしないでください。プライマリはレプリカが要求したオフセットがバックログリングにないと判断し、`TooOld` を返し、同じ RESP ワイヤ接続でシャードのキー空間スナップショットをインラインで送り、スナップショット末尾オフセットでライブフレームを再開します。レプリカはスナップショットを差し替え、ライブ末尾を適用し、追いつきます。空から再構築したければ、レプリカを止め、データディレクトリを削除して再起動。ランナーは `from_offset = 0` で接続し、キー空間全体をスナップショット送出します。

## 関連項目

- [`docs/availability.md`](../availability.md) — 運用の半分: トポロジ、整合性ラダー、計画 + クラッシュフェイルオーバー、エラー契約。本ページは機構(ワイヤ、フレーム、スナップショット送出)、あちらは何を実行しクライアントに何が見えるか。
- [`docs/cluster.md`](cluster.md) — マルチシャード公開とスロットルーティングの `ClusterClient`。レプリケーションと直交し、組み合わせ可能。
- [`docs/persistence.md`](persistence.md) — RDB と AOF。スナップショット送出パスはオンディスク形式をワイヤ上で再利用する。
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) — 読み書き分離クライアント。
- [`crates/kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) — クォーラムフェイルオーバー。
- [`crates/kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) — レプリカとして組み込む `Store::open_replica`。

## プライマリとして組み込む(v3.2)

組み込みアプリケーションが PRIMARY になり、kevy サーバーをそのレプリカにできます — プロセス内ストアに読み出しスケーリングとフルクエリ面(レプリカはレプリケートされたデータの上に自前のインデックス/ビュー/集約を宣言する)を与えます。

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

`single_source = true` はサーバーに、upstream がサーバー↔サーバーレプリケーションのシャードごとのポート群ではなく、単一のストリーム(組み込みライタソース)であることを伝えます。1 つのランナーが接続し、キー付きフレームはキーのハッシュでローカルシャードにルーティングされ、FLUSHALL/FLUSHDB はブロードキャストされ、スナップショット送出はペイロード全体をブロードキャストして各シャードは自分のハッシュスライスだけをロードします。

オフセット 0(フレッシュ)から、またはバックログウィンドウを過ぎた位置からハンドシェイクするレプリカは、組み込みソースからフルスナップショット送出を受けます(v1.21 のアンチスコープ、v3.2 でクローズ): 全シャードのポイントインタイムのフリーズと as-of オフセット、続いてライブフレーム。

CDC フィード([docs/cdc.md](../cdc.md))との関係: 両者は設計上共存します。レプリケーションソースはレプリカの整合性(インフラプレーン、ソースごとのオフセット)を、フィードはアプリケーション CDC((generation, offset) カーソル、プレフィックスフィルタ、at-least-once)を担います。両者を統合すると、アプリ向け CDC のセマンティクスがレプリカプロトコルに縛られてしまいます。

ゲート: `bench/repligate.sh` — 真の 2 プロセス: フレッシュなレプリカへのスナップショット送出、静止後のダイジェスト安定性、再起動の再同期、そしてレプリケートされたデータ上でのレプリカローカルな `IDX.CREATE`/`IDX.QUERY`。
