# レプリケーション

kevy がプライマリから 1 つ以上のレプリカに書き込みをストリーミングする仕組み、手動またはクォーラムによるフェイルオーバーの方法、そして組み込みプロセスが読み取りレプリカと同じストリームに subscribe する方法について説明します。

## このドキュメントが必要になるとき

次のいずれかに当てはまるときにレプリケーションを使います。

- **読み出しのファンアウト。** 1 台のプライマリがすべての書き込みを受け、1 つ以上のレプリカが読み出し負荷を吸収して [`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) クライアントの後ろでラウンドロビン。
- **HA フェイルオーバー。** 現プライマリが落ちたとき、生き残ったレプリカが自動で新プライマリを選出してほしい。クォーラムベースの昇格には [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) を足します。そうでなければ `REPLICAOF NO ONE` で手動昇格してください。
- **レプリカとして組み込む。** アプリケーションが [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) をプロセス内キー空間として使い、真実の源は `kevy` サーバーに置きたい。組み込みはプライマリをインメモリでミラーし、ネットワーク往復ゼロで読み出しを返します。書き込みはローカルでは拒否され、プライマリへ送る必要があります。

`kevy` ノードが 1 台しかないなら本書は不要です。クロス DC アクティブ・アクティブ、ゴシップディスカバリ、オンラインリシャーディング、Raft、AUTH、TLS が必要なら、kevy はそれらを永久に提供しません — 別のシステムを選んでください。

## 中心となる考え方

プライマリの `kevy` はシャードごとに専用のレプリケーションリスナーを開きます。適用された各変更は RESP エンベロープ(`*2\r\n:<offset>\r\n<argv>`)として、単調増加する 64 ビットのオフセットを伴ってエンコードされ、シャードごとの有界リングバックログにプッシュされます。各レプリカは最後に ack したオフセットからストリーミングします。要求オフセットがバックログから流れ去っていれば、プライマリはそのシャードのキー空間のスナップショットをインラインで送り、そのまま隙間なくライブストリーミングへ戻ります。レプリカはランタイムに `REPLICAOF host port` でターゲットを切り替えられ、`REPLICAOF NO ONE` で自身を降格できます。チェーンレプリケーション(レプリカのレプリカ)はワイヤ上サポートされず、適用パスで防御的に拒否されます。

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
listen_port_base = 16004        # シャード i は listen_port_base + i でレプリケーションを bind
replication_buffer_size = 268435456   # シャードあたり 256 MiB のリングバックログ
reconnect_window_ms     = 60000       # 再接続するレプリカのスロットをどれくらい保持するか
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

すべてのランナースレッドが止まり、有効ロールが `master` に flip します。ローカルデータは最後に適用されたフレームのまま残ります。下流レプリカを受け入れるには、設定も編集して(`role = "primary"` + `listen_port_base`)再起動する必要があります — ランタイムの `REPLICAOF NO ONE` は下流リスナーを bind しません。

### 5. ロールをプローブする

```sh
redis-cli -p 6004 ROLE
# 1) "master"
# 2) (integer) 12345678
# 3) 1) 1) "10.0.0.21"
#       2) (integer) 6004
#       3) (integer) 12345670

redis-cli -p 6004 INFO replication
# role:master
# connected_slaves:1
# master_repl_offset:12345678
# slave0:ip=10.0.0.21,port=6004,offset=12345670
```

`REPLICAOF` のライブランタイム状態は、応答中の静的 config よりも常に優先されます。

### 6. レプリカとして組み込む(ワンライナー)

アプリケーションは [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) を使ってプロセス内で同じレプリケーションストリームに参加できます。

```rust
use kevy_embedded::Store;

let store = Store::open_replica("primary.internal:16004")?;
assert!(store.is_replica());

// ローカル書き込みは READONLY で拒否される。
assert!(store.set(b"local", b"nope").is_err());

// 読み出しはネットワーク往復ゼロ — キー空間はこのプロセス内にある。
if let Some(v) = store.get(b"hello")? {
    println!("{:?}", v);
}
```

組み込みは同じ `listen_port_base` のシャードに接続し、到着順にフレームを適用し、ローカル arena から直接読み出しを返します。実行可能なコピーは [`crates/kevy-embedded/examples/replica.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/replica.rs) にあります。

## ノブ

サーバー側 TOML、`[replication]` 配下のキー:

| キー | デフォルト | 意味 |
|---|---|---|
| `role` | `"primary"` | `"primary"` はレプリケーションリスナーを開く。`"replica"` は `upstream` から引くランナーを spawn。 |
| `listen_port_base` | `16004`(プライマリ) | プライマリのシャード `i` は `listen_port_base + i` でレプリケーションを bind。レプリカは同じオフセットへ接続。 |
| `upstream` | 未設定 | レプリカ専用。プライマリの `listen_port_base` の `host:port`。各ローカルシャードは `(host, port + shard_index)` を狙う。 |
| `replication_buffer_size` | `268435456`(256 MiB) | バイト単位のシャードごとリングバックログ。この窓内の再接続はライブパスに留まる。古いオフセットはスナップショット送出をトリガ。 |
| `reconnect_window_ms` | `60000` | プライマリが切断レプリカのオフセット用スロットを回収するまでに予約しておく時間。 |

[`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) を構成する場合、`[cluster]` ブロックがクォーラムノブを足します。

| キー | デフォルト | 意味 |
|---|---|---|
| `node_id` | 未設定 | このノードの安定 ID(≤ 32 B ASCII)。選挙のタイブレーカに使用。 |
| `elect_port_base` | 未設定 | ハートビートと投票用のコントロールプレーン TCP ポート。シャード 0 は `elect_port_base + 0` を bind。 |
| `peers` | 空 | 自分を含む全クラスタノードの `id@host:port,…`。空ならエレクターは休眠。 |
| `hb_interval_ms` | `200` | ピア宛アウトバウンドハートビートの周期。 |
| `down_after_ms` | `5000` | ハートビートが途絶えてからピアが DOWN とフラグされるまでのミリ秒。 |
| `election_timeout_ms` | `3000` | 候補者がクォーラム `ACCEPT` を待つ時間。 |

クォーラムは `N/2 + 1` です。N=2 では両ノード生存が必要(どちらかが DOWN だと生存側は読み取り専用にロック)。リンターは警告を出します。フェイルオーバーが必要なデプロイでは N ≥ 3 を使ってください。

## トレードオフと限界

レプリケーションは**非同期**です。プライマリはどのレプリカがフレームを適用したか知る前にコミットして返信します。レプリカは、フレームがワイヤを渡ってシャードごとのチャネルを抜けて適用パスに入るまでの時間ぶん遅れます。`WAIT` 風のバリアも同期モードもありません。

| 関心事 | 答え |
|---|---|
| 書き込み耐久性 | ローカルストアとバックログリングに着地次第プライマリが ack。レプリカは後で追いつく。 |
| 読み出し整合性 | レプリカは遅れる可能性がある。read-after-write が重要なら `kevy-cluster-rw` 経由で `request_read(…, consistent = true)` を送ってプライマリで読む。 |
| レプリカが遅れすぎる | 再接続のオフセットがリングから流れ去っていれば、プライマリがそのシャードのスナップショットをインラインで送り、スナップショット末尾オフセットでライブフレームを再開 — 隙間なし、オペレータ操作なし。 |
| バックログのサイジング | `replication_buffer_size ≈ peak_writes_per_sec × avg_argv_bytes × reconnect_window_seconds`。大きすぎるのは無害、小さすぎるとスナップショット送出に落ちる。 |
| 何がフェイルオーバーするか | 新プライマリへの書き込み。`kevy-elect` 構成時は自動、それ以外は手動。既存の `kevy-cluster-rw` クライアントは新プライマリを学習次第書き込みを再ルーティング。間隙中の in-flight 書き込みは大きく失敗する。 |
| 何がフェイルオーバーしないか | クロス DC トラフィック、ゴシップで発見したピア、オンラインリシャーディング、AUTH/TLS — kevy はそれらをいずれも提供しない。シングル DC のみ。 |
| チェーンレプリケーション | ワイヤ上なし。レプリカの適用パスは下流に再発行しない。誤設定は防御的に拒否される。 |
| 分断中のマイノリティ側書き込み | 失われる。分断したマイノリティはクォーラムに届かず昇格できず、分断が癒えると降格してマジョリティの履歴を受け入れる。書き込み側で consistent-read パスを使って古い読み出しを避ける。 |

ワイヤフォーマット(ライブフレームエンベロープ、スナップショット送出、ハンドシェイク)は [`crates/kevy-replicate/docs/wire.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/wire.md) と [`crates/kevy-replicate/docs/snapshot.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/snapshot.md) に文書化されています。エレクターのプロトコルは [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md) にあります。

## FAQ

**レプリカをどう昇格しますか?**
手動: レプリカに繋いで `REPLICAOF NO ONE`。有効ロールは即時に `master` に flip し、ローカルストアは保たれ、書き込みが受け入れられます。下流レプリカを受けるには TOML の `role` と `listen_port_base` も更新して再起動してください。自動: 全ノードで `node_id`、`elect_port_base`、`peers` リストを設定して `kevy-elect` を構成。`repl_offset` 最大の生存レプリカがクォーラムで勝ちます。

**レプリカがプライマリになり、さらにレプリカに戻れますか?**
はい。`REPLICAOF NO ONE` はデータに触れず upstream リンクだけ降格します。続く `REPLICAOF host port` で新プライマリへ再アタッチ。両方の遷移をまたいでローカルストアは保持されます。新 upstream からクリーンリプレイしたければ事前に `FLUSHALL` してください。

**データロス窓は?**
「プライマリがクライアントに ack する」から「すべてのレプリカがフレームを適用した」までの間隔です。レプリケーションは非同期なので、書き込みを ack した直後にプライマリがクラッシュし、どのレプリカもまだフレームを持っていなければ、その書き込みは失われます。窓のサイジングはワークロード依存 — シングル DC LAN ではたいていサブミリ秒です。同期モードはありません。電源断をまたぐ耐久性が必要なら、プライマリ側で [`docs/persistence.md`](persistence.md)(AOF + RDB)とレプリケーションを併用してください。

**レプリカから読めますか?**
はい — それがレプリカを足す主な理由です。[`kevy-cluster-rw::ReadWriteClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) を使い、書き込みはプライマリへ、読み出しは渡したレプリカシードでラウンドロビンします。直近書き込みを必ず観測したい読み出しは、同じクライアントの consistent-read パスでプライマリ経由に強制します。

**レプリカが遅れすぎてしまいました — どう復旧しますか?**
何もしないでください。プライマリはレプリカが要求したオフセットがバックログリングにないと判断し、`TooOld` を返し、同じ RESP ワイヤ接続でシャードのキー空間スナップショットをインラインで送り、スナップショット末尾オフセットでライブフレームを再開します。レプリカはスナップショットを差し替え、ライブ末尾を適用し、追いつきます。空から再構築したければ、レプリカを止め、データディレクトリを削除して再起動。ランナーは `from_offset = 0` で接続し、キー空間全体をスナップショット送出します。

## 関連項目

- [`docs/cluster.md`](cluster.md) — マルチシャード公開とスロットルーティングの `ClusterClient`。レプリケーションと直交し、組み合わせ可能。
- [`docs/persistence.md`](persistence.md) — RDB と AOF。スナップショット送出パスはオンディスク形式をワイヤ上で再利用する。
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) — 読み書き分離クライアント。
- [`crates/kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) — クォーラムフェイルオーバー。
- [`crates/kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) — レプリカとして組み込む `Store::open_replica`。
