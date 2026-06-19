# クラスタ・モード + クラスタ対応クライアント(kevy-client 1.9.0)

kevy は **シングル・ノード、マルチ shard** エンジンです。クラスタ・
モードはマルチ・ホスト分散ではありません(failover、gossip、オンライ
ン resharding、MIGRATE/ASK はない —— それらは永久にスコープ外)。各
内部 shard をアドレス可能なクラスタ・ノードとして公開する方法で、key
対応クライアントが **key を所有する shard と直接対話**、サーバー側の
クロス shard 転送ホップをスキップします。

そのホップが全ての要点:デフォルトの単一ポート・プロキシ動作下、間違っ
た shard に着いたコマンドは内部的に owner に転送されます。その転送は
正しいですがコストがかかる —— 低負荷ではテール・レイテンシ、高負荷で
はスループットを支配します(計測:下の[性能](#性能)参照)。クラスタ・
モード + ルーティング・クライアントがそれを取り除きます。

## サーバー側 —— `--cluster`

```sh
kevy --threads 8 --cluster          # メインポート 6004、shard ポート 6005-6012
```

`--cluster`(または `KEVY_CLUSTER=1`、または `[cluster] enabled =
true`)は 3 つのことをします:

- **per-shard リスナー**。shard `i` は `port + 1 + i` の決定論的な追
  加ポートを取得します(`[cluster] port_base` で base を上書き)。メ
  イン・ポートは他のすべてに対して完全なプロキシ風動作を保持します。
- **本物のトポロジー報告**。`CLUSTER SLOTS / SHARDS / NODES` は実際の
  パーティションをアドバタイズ:CRC16 `{hashtag}` slot、shard 毎に 1
  つの連続レンジ。`CLUSTER KEYSLOT` / `COUNTKEYSINSLOT` / `MYID` は実
  装済みで、上流 Redis と一致します。
- **転送ではなく `-MOVED`**。クラスタ・ポートに着いた間違った shard
  の key はプロキシされる代わりに `-MOVED <slot> <host:port>` を返し
  ます。正しいルーティングは `-MOVED` が決して発火しないことを意味し
  ます。

既存のデータ・ディレクトリをクラスタ・モードに切り替えると、起動時に
1 度 key を re-home します;ソース・ファイルは `*.premigration.<ts>`
としてバックアップされます。

ストックのクラスタ対応ツール —— `redis-cli -c`、
`redis-benchmark --cluster`、主流のクライアント・ライブラリ —— はプロト
コル・サブセットが忠実なため、クラスタ・ポートに対して直接動作します。

## クライアント側 —— `ClusterClient`

`kevy-client` 1.9.0 はタイプ付きルーティング・クライアントを ship し
ているので、フルのサードパーティ・クラスタ・ライブラリは不要です:

```toml
[dependencies]
kevy-client = "1.11"
```

```rust
use kevy_client::ClusterClient;

// 任意のクラスタ・ポートを seed として接続;トポロジーは CLUSTER SLOTS
// で発見され、shard 毎に 1 接続が open されます。
let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;

cc.set(b"user:42", b"alice")?;
let v = cc.get(b"user:42")?;            // user:42 の owner shard にルーティング
let n = cc.incr(b"counter")?;

// マルチ key DEL/EXISTS は shard を跨ぐかも —— key 毎にルーティングして合算。
let removed = cc.del(&[b"a", b"b", b"c"])?;
# Ok::<(), std::io::Error>(())
```

実行可能版は
[`crates/kevy-client/examples/cluster.rs`](../../crates/kevy-client/examples/cluster.rs)
にあります:

```sh
kevy --port 6004 --threads 4 --cluster          # shard は 6005-6008
cargo run -p kevy-client --example cluster -- 6005
```

### ルーティングの仕組み

1. **発見**。`connect` は seed に `CLUSTER SLOTS` を送り、各 shard の
   `[start, end, host, port]` を返します。クライアントは `slot →
   shard-index` テーブル(16384 エントリ)を構築し、各別 shard ノード
   に対して 1 つの `RespClient` を open します。テーブルはサーバーの
   *実際の* アドバタイズ・レンジから来るので、クライアントはサーバー
   の `slot → shard` 演算を複製する必要はありません。
2. **ルーティング**。各単一 key コマンドは `key_hash_slot(key)`
   (`{hashtag}` があれば CRC16 XMODEM、なければ全体の key)を計算し、
   その slot の owner 接続にリクエストを送ります。`-MOVED` なし、転送
   なし。
3. **必要なときは扇出**。`DBSIZE` / `FLUSHALL` はクラスタ全体 —— kevy
   はこれをサーバー側で扇出します(`Route::Dbsize` /
   `Route::Flush`)、なので 1 回の呼び出しでクラスタ全体を報告/wipe
   します;クライアントは自身で合算しません。

### コマンド・カバレッジ

| グループ | コマンド |
|---------|--------|
| String | `set`、`set_with_ttl`、`get`、`incr`、`incr_by`、`expire`、`persist`、`ttl_ms` |
| Keys(マルチ、key 毎にルーティング) | `del`、`exists` |
| クラスタ全体(サーバー扇出) | `dbsize`、`flushall` |
| key なし | `ping`、`publish` |
| Hash | `hset`、`hget`、`hdel`、`hlen`、`hgetall`、`hkeys`、`hvals` |
| List | `lpush`、`rpush`、`lpop`、`rpop`、`llen`、`lrange` |
| Set | `sadd`、`srem`、`smembers`、`scard`、`sismember`、`sinter`、`sunion`、`sdiff` |
| Sorted set | `zadd`、`zrem`、`zscore`、`zcard`、`zrange` |

ラップされていないものには、生のルーティング・ヘルパーに落とします:

```rust
// 任意の単一 key コマンドを owner shard にルーティング。
let reply = cc.request_keyed(b"mykey", &[b"STRLEN".to_vec(), b"mykey".to_vec()])?;
// 任意の shard が応答する key なしコマンド。
let reply = cc.request_unkeyed(&[b"PING".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

### マルチ key の same-slot 制限

集合結合操作(`sinter` / `sunion` / `sdiff`)は **最初の** key でルー
ティングします。Redis Cluster と同様、すべての key は同じ slot に存在
する必要があります —— 一緒にハッシュされるように共有 `{hashtag}` を使
用:

```rust
cc.sadd(b"{users}:active",  &[b"a", b"b"])?;
cc.sadd(b"{users}:premium", &[b"b", b"c"])?;
let both = cc.sinter(&[b"{users}:active", b"{users}:premium"])?; // 同じ slot → OK
# Ok::<(), std::io::Error>(())
```

共有 hashtag なしでは key が異なる shard に着地し、サーバーが
`-MOVED` を返します(`io::Error` として現れる)。`del` / `exists` は
そう制約 **されません** —— それぞれ独立にルーティングして結果を合算
します。

Pub/sub にはクラスタ対応 subscriber は **不要**:kevy の pub/sub はプ
ロセス全体(任意の shard で publish されたメッセージはすべてのコアの
subscriber に配信)で、任意の単一ポートに接続した通常の `Subscriber`
ですべてのメッセージが見えます。`ClusterClient::publish` も同様に 1
つの shard にだけ送ります。

## 性能

クリーンな lx64 16 コアのベアメタル・ボックスで計測、サーバーとクライ
アントは別コア、並行 64 の GET ワークロード:

| クライアント・パス | スループット | p99 レイテンシ |
|------------------|----------:|-----------:|
| シングル shard プロキシ(クロス shard ホップ) | 333 k ops/s | 3858 µs |
| **`ClusterClient`(ゼロ・ホップ)** | **533 k ops/s** | **260 µs** |

それは **スループット 1.6 倍、テール・レイテンシ約 15 倍低下** ——
純粋に転送ホップを取り除いただけで。タイプ付き `ClusterClient` は手書
き生 socket ルーターと同じ天井に達するので、タイプ付き API は計測可能
なオーバーヘッドを追加しません。再現:
`cargo run -p kevy-client --release --example cluster_bench`。

> perf bench はクリーンでコア隔離されたマシンで実行してください。小規
> 模な同居クラウド VM ではクロス shard ホップのコストがスケジューリン
> グ・ノイズに埋もれます —— ホップが重要ではないと結論づける調査をほ
> ぼ誤導しました。

## いつ使うか

- **`ClusterClient` を使う** —— 単一クライアントが転送ホップが目立つ
  ほどの負荷を押し出すとき —— 高スループットまたはテール・レイテン
  シ・センシティブなワークロード。負荷下で kevy を自己ホストする推奨
  パスです。
- **通常の `Connection` / 単一ポートに留まる** —— 通常使用:プロキシ動
  作は正しくシンプル、低負荷ではホップは安価。
- **`redis-cli -c` / サードパーティのクラスタ・クライアントに手を伸
  ばす** —— 相互運用テストのみ;ネイティブの `ClusterClient` は Rust
  caller には軽量です。

## 読み書き分離:クラスタ・モードとレプリケーションの組み合わせ(v1.18)

`kevy-cluster-rw` は **レプリケーション** トポロジー用の兄弟クライア
ント —— 書き込みを処理する primary kevy ノード + 読み取りを処理する
replica kevy ノードのフリート(サーバー側は
[`docs/replication.md`](replication.md) 参照)。これは **クラスタ・モー
ドに直交**:レプリケーション・トポロジーは *プロセス* 毎に 1 writer、
クラスタ・モードは 1 プロセスを N shard に分割。組み合わせ可能 ——
クラスタ・モードの primary は N shard をアドバタイズ、各 replica も
N shard を実行し、オペレータが間に `kevy-cluster-rw` を配線します。

```rust
use kevy_cluster_rw::ReadWriteClient;

let mut client = ReadWriteClient::connect(
    ("primary.local", 6004),
    &[("replica1.local", 6004), ("replica2.local", 6004)],
)?;
// 書き込み → primary、読み取りは replica で round-robin(フリートが空
// の時 primary にフォールバック)。`consistent = true` で読みを
// primary に強制。
client.request(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?;
let reply = client.request(&[b"GET".to_vec(), b"k".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

この crate は v1.18 リリースで追加。コマンド毎の読み/書き分類は
`kevy_cluster_rw::is_write_verb` にあります。v1.18 は seed リストを明
示的に取ります(自動 `CLUSTER NODES` ディスカバリなし —— リリース後
の follow-up);オペレータのデプロイ・スクリプトが primary + replica
アドレスをリストします。

## embed-as-read-replica(v1.20 / Phase 2)

`kevy-embedded` ストアはサーバー primary のレプリケーション・ストリー
ムを subscribe してプロセス内で keyspace をミラーできます。読み取り
はネットワーク round-trip ゼロ;書き込みはローカルで拒否され、primary
に送る必要があります。

```rust
use kevy_embedded::Store;

// 1 行:インメモリ replica、AOF オフ、デフォルト reconnect(100 ms → 5 s)。
let replica = Store::open_replica("primary.local:16004")?;

// 読み取りは動作;書き込みは io::Error("READONLY ...") を返す。
let v = replica.get(b"hello")?;
assert!(replica.set(b"k", b"v").is_err());
# Ok::<(), std::io::Error>(())
```

完全な制御:

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

ハンドシェイクは `REPLICATE FROM <last-applied-offset> ID
<replica_id>` を primary のレプリケーション・リスナー(サーバー側のデ
フォルトは `port + 10000`、サーバーの `--replication-listener` /
`[replication] listen_port` で設定)に送信します。primary は offset
を ack してフレームのストリーミングを開始;埋め込み runner スレッド
はサーバー側 replica が使うのと同じ dispatch パスで各フレームをロー
カル shard に適用し、再接続時の resume のためローカル offset を進め
ます。

### v1.20 スコープ(MVP)

- **単一 upstream URL = 単一 primary shard ミラー**。マルチ shard
  upstream は今のところ "primary shard 毎に 1 つの
  `Store::open_replica` を spawn";per-URL runner 利便面は follow-up
  で着地します。
- **replica にローカル AOF なし**。`open_replica` は強制無効化します
  (ローカル AOF は再起動を跨いで発散し次の open で二重適用)。
  replica の再起動を跨いだ耐久性のためには、upstream の backlog を
  replica の last-applied offset がディスクに残るほど長く保持してくだ
  さい。
- **snapshot ingest なし**。replica が offset 0 で、backlog がその点
  を超えて進んだ primary に接続すると現在接続を切断します;フルの
  snapshot ingest(`+SNAPSHOT ... +SNAPSHOT_END`)は v1.20.x follow-up。
- **`kevy-elect` ANNOUNCE による自動再ターゲットなし**。primary 変
  更には failover フックが着地するまで手動再設定;`kevy-cluster-rw`
  のトポロジー・リフレッシュと組み合わせて完全自動化されたパスへ。
- **replica 上のローカル PUBLISH 許可**。Pub/sub は kevy ではプロセ
  ス・ローカル(レプリケートされない)なので、ローカル PUBLISH はこ
  のプロセスの subscriber にだけ到達します;keyspace 自体は読み取り
  専用のままです。

### 故障モード

- **Primary down** —— runner は指数バックオフで reconnect
  (`Config::with_replica_reconnect`、デフォルト 100 ms → 5 s)。読み
  取りは最後に適用された snapshot に対して動作し続けます;書き込みは
  まだ `READONLY` を返します。
- **オフセット・ギャップ** —— ワイヤ・クライアントが `OffsetGap` を
  surface;runner は接続を切断し、次の reconnect が新しい applied
  offset から拾い上げます(これは primary に遅れた状態)。v1.20.x
  snapshot ingest がこのギャップを自動的に閉じます;v1.20 はオペレー
  タが snapshot から手動でリフレッシュする必要があります。
- **Replica drop** —— runner スレッドは最後の `Store` clone drop で
  join されます;primary のリスナーはクリーンな FIN を観察し、
  per-replica スロットを解放します。

## scope 別マルチ writer(v1.21 / Phase 3)

`kevy-scope` でオペレータがプリフィックス単位の所有権を宣言できます:
特定の writer ノードが `<prefix>` にマッチする key の書き込みを所有
し、他のすべてのノードは `-MISDIRECTED writer is <host:port>` を返
し、クライアントが follow します。オプションのフォールバックが
`kevy-elect` が writer DOWN とフラグを立てたときに引き継ぎます。

```toml
[cluster]
node_id = "embed-billing-1"
elect_port_base = 16100
peers   = "embed-billing-1@10.0.0.1:6004,server-eu-1@10.0.0.2:6004,reader-1@10.0.0.3:6004"
# prefix=writer[|fallback]、カンマ区切り。prefix 内の埋め込み `:` は
# OK(最初の `=` が prefix と owner spec を分割)。
scopes  = "app:billing:=embed-billing-1|server-eu-1, app:auth:=embed-auth-1"
```

### 反スコープ(v3-cluster RFC でロック)

- **Raft なし、gossip なし**。所有権テーブルは静的 config;elect ク
  ォーラムは "writer DOWN → fallback 引き継ぎ" のみシグナル、トポロ
  ジー・コンセンサスではない。
- **マイグレーション中の write-shadow なし**。`MOVE-SCOPE` は Q3=(a)
  quiesce-window として実行:writer はプリフィックスの書き込みを一時
  停止し、その slice を ship、その後所有権が翻転。オペレータ協調、
  二重受容ウィンドウなし。
- **自動マイグレーションなし**。`MOVE-SCOPE` はオペレータ発行;クラス
  タが自分で scope を移動すると決めることはない。

### ワイヤ形状

- `-MISDIRECTED writer is <host:port>` —— 書き込みが scope の writer
  (またはアクティブなフォールバック)でないノードに着地。
  `kevy-cluster-rw` 1.21+ は key 毎ターゲットをキャッシュし透過的に
  retry;v1.20 以前のクライアントはエラーを伝播します。
- `-QUIESCED migrating to <host:port>` —— MOVE-SCOPE ウィンドウ中の
  一時的なもの。クライアントは panic せず短時間バックオフして retry
  すべきです;quiesce ウィンドウは slice ship 時間で制限されます
  (LAN 上の GB クラス scope で 1 桁秒)。

### Embed を writer に

scope の writer は埋め込み(`embed-as-writer`)またはサーバーになり
得ます。埋め込みの場合:

```rust
use kevy_embedded::{Config, Store};

let writer = Store::open(
    Config::default().with_embed_writer("0.0.0.0:6105")
)?;
// 各ローカル書き込みは埋め込みのレプリケーション・ソース・バックロ
// グにプッシュ;reader は `kevy_replicate::ReplicaClient` 経由で
// `0.0.0.0:6105` に接続。
writer.set(b"app:billing:invoice:42", b"...")?;
# Ok::<(), std::io::Error>(())
```

### F4 フォールバック

`kevy-elect` が scope の writer を `down_peers` に報告するとき
(最後の HB が `down_after` よりも古い、デフォルト 5 s)、宣言された
フォールバックは自身をアクティブな owner として扱います。フォール
バック上の書き込みは成功;他のすべてのノード上の書き込みは引き続き
MISDIRECT、今はフォールバックを指名します。writer の HB が再開すると
自動 reclaim は暗黙 —— writer が `down_peers` を離れるので、フォー
ルバックが退きます。

**手動再参加リカバリ(v1.21)**。writer がフォールバックが書き込み
を受け入れるほど長く DOWN だった場合、それらの書き込みはフォール
バック上にしか存在しません。scope の writer を再有効化する前に:
writer を停止、フォールバックのデータ・ディレクトリを writer のに
コピー、writer を再起動。v3.1 はフォールバックのストリームからの
writer-replica ハンドシェイクでこれを自動化;v1.21 は「凝ったコン
センサスなし」の契約内に留めるため手動を保ちます。

### MOVE-SCOPE

```
MOVE-SCOPE <prefix> from <from-node-id> to <to-node-id>
```

ソース writer に対して発行。Q3=(a) quiesce ウィンドウを歩きます:

1. writer は `<prefix>` のローカル・マイグレーション状態を
   MIGRATING に翻転;以降そのプリフィックス下の key への書き込みは
   `-QUIESCED migrating to <to-host:port>` を返します。
2. writer はプリフィックスの keyspace slice をシリアライズし、
   `MOVE-SCOPE-INGEST <prefix> <bulk>` 経由でターゲットのデータ・
   ポートに ship します。
3. `+OK` で、writer はローカルでマイグレーションをコミット:ソース
   上のプリフィックスへの今後の書き込みは
   `-MISDIRECTED writer is <to-host:port>` を返します(quiesce なし
   —— 移動完了)。
4. 他のクラスタ・メンバーは、オペレータが新しい config を push +
   再起動するまで、静的な `[cluster] scopes` config に従ってルーティ
   ングを続けます(v1.21 は gossip なし)。

**制限(v1.21)**:
- マイグレーション状態は per-node ローカル。他のクラスタ・メンバー
  は新しい writer を学ぶために config push + 再起動が必要。
- データ ship はプリフィックス slice 全体をメモリ内でシリアライズ。
  プリフィックスが ≫ GB の場合、メンテナンス・ウィンドウ中に
  MOVE-SCOPE をスケジュール;embed-as-writer の MVP はそのスケールに
  サイズされていません。
- 途中での中止はソース writer に戻ります;ターゲットに部分適用状態
  は残りません。
