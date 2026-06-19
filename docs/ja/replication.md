# プライマリ・レプリカ・レプリケーション + 読み書き分離クライアント(v1.18 / v3-cluster Phase 1)

kevy v1.18 は v3-cluster の **Phase 1 機能コア** を出荷:kevy ノードは
適用した各 mutation を N 個の read replica にストリーミングする
primary、または primary に接続して keyspace をミラーする replica と
して動作可能。新しいクライアント crate `kevy-cluster-rw` は書き込みを
primary に、読み取りを replica に round-robin します。

**スコープ外の確認**(plan でロック済み;v1.18 issue に**入れない**こと):

- マルチ master / sharded-multi-master —— scope 毎に writer 1 つだけ。
- クロス DC active-active / CRDT。
- Raft / 強ログ・レプリケーション。
- オンライン resharding / gossip ディスカバリ —— peer リストはオペ
  レータ宣言。
- AUTH / TLS —— kevy のスコープ外で永久。
- チェーン・レプリケーション(replica-of-replica) ——
  dispatch-without-emit ゲートは誤設定への防御ですが、ワイヤ形状は 1
  ホップのみサポート。

自動クォーラム・フェイルオーバー(`kevy-elect`)は Phase 1.5、
v1.18 には **入っていません**。手動昇格は `REPLICAOF NO ONE` 経由で
v1.18 の failover 面です。

## サーバー側

### Primary

```toml
# kevy.toml
[replication]
role = "primary"
listen_port_base = 16004      # shard i はこの + i でレプリケーションを bind
replication_buffer_size = 268435456   # shard 毎 256 MiB リング backlog
reconnect_window_ms = 60000   # replica の offset 用スロットをこの長さ保持
```

shard `i` は `listen_port_base + i` に専用レプリケーション TCP
listener を bind します(Issue Ledger I2 —— per-shard cluster
listener パターンをミラー)。適用された各書き込みは RESP エンベロー
プ(`*2\r\n:<offset>\r\n<argv>`)にエンコードされ、shard 毎の有界リン
グ backlog にプッシュされます;reactor の pump がイテレーション毎に
これらのフレームを接続中の各 replica にストリーミングします。

プロトコルは RESP3 拡張([`crates/kevy-replicate/docs/wire.md`])。
オフセットは `i64` エンコード;1000 万 writes/s で i64::MAX 上限は
≈ 30 000 年先。

### Replica

```toml
[replication]
role = "replica"
upstream = "primary.example:16004"    # primary の listen_port_base
```

kevy が `role = "replica"` で起動すると、サーバーはローカル shard
毎に **runner スレッド** を 1 つ spawn します。runner `i` は
`(upstream_host, upstream_port_base + i)` への blocking TCP 接続を
open、ハンドシェイク(`REPLICATE FROM <offset> ID <replica_id>`)を送
信、`+ACK <offset>` を読み、ワイヤ・ストリームをループします。各
`ReplicaEvent`(live フレーム、または `SnapshotBegin` /
`SnapshotChunk` / `SnapshotEnd` のいずれか)は MPSC チャネル経由で対
応する shard の reactor スレッドに転送されます;shard はチャネルを
tick 毎に 1 度ドレインし、`ReplicatedApplyGuard` スコープ内で通常の
dispatch パスを通して適用します。

ガードは適用中ローカルの `ReplicationSource::push_mutation` を抑制し
ます —— これがないと、downstream listener が設置された replica が適用
した各フレームを再発行し、offset を二重計上します。v1.18 はチェーン・
レプリケーションを禁止;ゲートは防御的です。

スナップショット出荷:replica がリクエストした `from_offset` が
primary backlog にもうない(TooOld)場合、primary は
`kevy_persist::write_snapshot_to` 経由で shard の keyspace を in-line
シリアライズし、`+SNAPSHOT\r\n` でプレフィックスし、`$<chunk>\r\n`
bulk をストリームし、`+SNAPSHOT_END <ack_offset>\r\n` で終了します。
replica はチャンクを蓄積し、`kevy_persist::load_snapshot_from` でロー
カル `Store` に読み込み、`ack_offset` から live フレームをギャップなし
で継続します。

## コマンド

| コマンド | 効果 |
|---------|------|
| `ROLE` | upstream が active でないとき `master <offset> []`、replica として動作中なら `slave <host> <port> connect 0`。`REPLICAOF` のライブ状態が静的設定より優先。 |
| `INFO replication` | role / connected_slaves / master_repl_offset(master)または master_host / master_port / master_link_status(replica)。 |
| `REPLICAOF host port`(エイリアス `SLAVEOF`) | 動作中の runner fleet を停止、新 upstream をパース・解決、新しい runner を spawn。`+OK` を返す。 |
| `REPLICAOF NO ONE` | 各 runner を停止;standalone に降格(ローカル store は **クリアされません** —— 昇格前に FLUSH するかはオペレータ判断)。 |
| `CLUSTER NODES` | 応答ノードの role フラグはライブのレプリケーション状態を反映(`myself,master` または `myself,slave`)。 |

## クライアント側 —— `kevy-cluster-rw::ReadWriteClient`

```rust
use kevy_cluster_rw::ReadWriteClient;

let mut client = ReadWriteClient::connect(
    ("primary.local", 6004),
    &[("replica1.local", 6004), ("replica2.local", 6004)],
)?;

// 自動ルーティング:SET は primary 行き、GET は replica で round-robin。
client.request(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?;
let reply = client.request(&[b"GET".to_vec(), b"k".to_vec()])?;

// READCONSISTENT —— 読みを強制的に primary に(書き直後の読み)。
let reply = client.request_read(
    &[b"GET".to_vec(), b"k".to_vec()],
    /* consistent = */ true,
)?;
```

v1.18 は seed リストを明示的に取ります —— replica ディスカバリのため
の自動 CLUSTER NODES 走査はありません。(リリース後の follow-up で
オペレータがクライアントに replica を自動発見させたい cluster モード
デプロイ向けに auto-discover オーバーロードを追加可能。)

書き/読みの分類は [`kevy_cluster_rw::is_write_verb`]。テーブルはサー
バー側の `kevy::cmd::is_write_verb` をミラーします;重複は意図的(こ
の crate は `kevy-resp-client` のみの下流 —— サーバー crate には決し
て依存しない)。

## 運用レシピ

### 新しい replica を追加

1. 新しい kevy を `[replication] role = "replica"` と
   `upstream = "primary:16004"` で起動。
2. runner は `from_offset = 0` で接続。primary の backlog は offset 0
   を既に evict 済み → TooOld → snapshot ship。
3. snapshot ロード後、runner は `ack_offset` で再開し live フレームに
   生き続けます。

### 動作中の replica を再ターゲット

```
REPLICAOF new-primary.example 16004
```

古い runner fleet を停止(ソケットは `Shutdown::Both` 済み —— in-
flight 読み込みは解除)、新 upstream をパース、新しい runner を
spawn。ミリ秒以内に `+OK` を返します。replica のローカル store は
**保持**されます —— 新 primary からのフレームがその上に降ってきます。
オペレータがクリーン replay を望むなら、前後に `FLUSHALL` を続けて
ください。

### 手動昇格(replica → primary)

```
REPLICAOF NO ONE
```

各 runner を停止。実効 role は `master` に反転。ローカル store は最
後に適用されたフレームが残した状態のままです。downstream replica を
受け入れるには、config(`role = "primary"` + `listen_port_base`)も
更新して再起動 —— v1.18 は downstream listener を **動的にはインストー
ルしません**。

## `kevy-elect` 経由の自動フェイルオーバー(v1.19+ / Phase 1.5)

v1.19 は v1.18 の手動 `REPLICAOF` の上にクォーラム・ベースの primary
フェイルオーバーを追加。検出はハートビート(`HB(epoch, node_id, role,
repl_offset)`)を `hb_interval_ms`(デフォルト 200 ms)毎に;peer は
`down_after_ms`(デフォルト 5 s)ハートビートなしで DOWN マーク;
最高 `repl_offset` を持つ alive replica(同点時は最小 `node_id`)が
`OFFER(new_epoch, candidate_id, repl_offset)` をブロードキャスト;
`N/2 + 1` `ACCEPT` を集めたら既存の `REPLICAOF NO ONE` パス経由で自身
を昇格し、`ANNOUNCE(epoch, new_primary_id, new_primary_addr)` をブロー
ドキャスト。`ANNOUNCE` を受信した peer は `kevy-replicate` runner を
新 primary に再ターゲットします。完全仕様:
[`crates/kevy-elect/docs/protocol.md`](../../crates/kevy-elect/docs/protocol.md)。

### 設定

```toml
[cluster]
node_id = "primary-east"              # このノードの安定 id(≤ 32 B ASCII)
elect_port_base = 16104               # 制御面 TCP ポート(shard 0 = base + 0)
peers = "primary-east@10.0.0.1:16104,replica-1@10.0.0.2:16104,replica-2@10.0.0.3:16104"
```

`peers` 文字列はクラスタの **すべての** ノード(このノード自身を含む)
をリストします —— elector が実行時に `node_id` で自己フィルタします。
空の `peers` ⇒ kevy-elect はドーマント(v1.18 時代の設定は変更不要)。

### クォーラムと耐障害性

| N | クォーラム | 耐 |
|---|----------|----|
| 3 | 2 | 1 down |
| 5 | 3 | 2 down |
| 7 | 4 | 3 down |
| **2** | **2** | **0 down —— 退化、意図的にロック** |

**N=2 警告**。クォーラムは `N/2 + 1` なので、N=2 は両ノードのアライ
ブが必要:どちらが down しても生存者はクォーラムに達せず、
**読み取り専用のまま** 無期限に居座ります(書き込み拒否、昇格なし)。
これは意図的 —— 代替案(シングル・ノード・クォーラム)はパーティショ
ン時に split-brain 二重書き込みのリスク。設定 linter は `peers` が
ちょうど 2 エントリの時に起動時に warning。**推奨:N ≥ 3** 自動
フェイルオーバーが必要な任意のデプロイで。N=2 は「どちらかが down =
ロック」が「両方が down = ロック」より望ましいときのみ受容可(極めて
稀)。

### Split-brain 保護

クォーラム・セマンティクスは構造的に split-brain を防ぎます:パーティ
ション少数側は `N/2 + 1` ACCEPT に達せず、新 primary を昇格できません。
パーティションが治癒すると、少数側は多数側からのより高い epoch を見て
クリーンに降格します —— パーティション中に少数側に着地した書き込みは
失われるコスト付き。これが v3-cluster Phase 1.5 が出荷する耐久性ストー
リー:**書き込みは任意のパーティションの多数側でのみ耐久性が保証され
ます**。Stale 読みを避けるには `READCONSISTENT`;書き込み側は遡って
少数側書き込みを修復できません。

### チューン可能項目

| パラメータ | デフォルト | 動作 |
|----------|---------|------|
| `hb_interval_ms` | 200 | peer 毎の outbound HB の周期 |
| `down_after_ms` | 5_000 | この ms HB なしで peer DOWN マーク |
| `election_timeout_ms` | 3_000 | candidate がクォーラム ACCEPT をこの長さ待つ |
| `election_backoff_ms` | 1_000–5_000 | 選挙失敗後のバックオフのランダム・ジッター |

`hb_interval` × `down_after` を RTT に合わせて調整。デフォルトは単一
LAN を想定。WAN デプロイ(v1.19 スコープ外 —— kevy-elect は単一 DC
のみ)は一時的な WAN ブリップ中の誤選挙を避けるため、より高い値が必
要です。

### Backlog チューニング

`replication_buffer_size` は shard 毎リングのバイト予算。サイジングの
親指則:

```
backlog_size ≈ ピーク 書き込み 毎秒 * 平均 argv バイト * reconnect ウィンドウ 秒
```

200k writes/sec、40 B 平均 argv、60 s ウィンドウで shard 毎 480 MiB
が全 reconnect を backlog パスに保ちます。小さい backlog でも問題なし
—— 過大なものは snapshot ship にクリーンにフォールバック。

## 既知の v1.18 単純化(follow-up として追跡)

- **バックグラウンド・スナップショット・シリアライゼーション** ——
  *v1.18 で着地*。primary は COW `SnapshotView` を凍結(O(n) shallow
  clone —— ns/entry)し、worker スレッドに渡して reactor 外でシリア
  ライズ;チャンクはチャネル経由でストリーム・バック。reactor のポー
  ズは collect のみに縮小。
- **per-replica peer-addr** —— *v1.18 で着地*。ROLE master 応答は接続
  中の各 replica に対して `(ip, port, offset)` を運びます;
  `INFO replication` の `connected_slaves` はこのリストから派生。
- **io_uring 上のレプリケーション** —— *v1.18 で着地*。io_uring
  reactor の tick パスが replica の accept / read / write / pump を駆
  動;スループット重要な書き込み側は io_uring ネイティブ(短書き込み
  + 既存の非ブロッキング drain)。`KEVY_IO_URING=1` + レプリケーショ
  ンが動作し、epoll reactor の perfgate 数値と一致。
- **CLUSTER NODES live-replica リスト** —— primary は現在、接続中
  replica のクライアント側アドレスを追跡しません(runner の REPLICATE
  ハンドシェイクは id のみ運ぶ)。クライアントは代わりに明示的な seed
  で `kevy-cluster-rw` を使用。
- **Auth / link 暗号化** —— ありえない(スコープ外)。

## ワイヤ・フォーマット参照

- ライブ・フレームのエンベロープ:[`crates/kevy-replicate/docs/wire.md`]。
- スナップショット出荷:[`crates/kevy-replicate/docs/snapshot.md`]。
- ハンドシェイク:`*5\r\n$9\r\nREPLICATE\r\n$4\r\nFROM\r\n$<n>\r\n<offset>\r\n$2\r\nID\r\n$<m>\r\n<replica_id>\r\n` → `+ACK <offset>\r\n`。

## 関連

- [`docs/cluster.md`](cluster.md) —— マルチ shard 露出 + slot ルー
  ティング `ClusterClient`;レプリケーションに直交(だが組み合わせ可
  能)。
- [`docs/persistence.md`](persistence.md) —— RDB / AOF;snapshot パス
  はワイヤ ship フォーマットに kevy-persist を再利用。
- `.claude/plans/2026-06-18-v3-cluster-plan.md` —— 正規実行計画;行の
  状態は本リリースに含まれるものを反映。

[`crates/kevy-replicate/docs/wire.md`]: ../../crates/kevy-replicate/docs/wire.md
[`crates/kevy-replicate/docs/snapshot.md`]: ../../crates/kevy-replicate/docs/snapshot.md
[`kevy_cluster_rw::is_write_verb`]: ../../crates/kevy-cluster-rw/src/lib.rs
