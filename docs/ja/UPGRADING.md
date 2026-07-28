# kevyのアップグレード

章は2つ、新しいものから。**3.x → 4.0**（APIの定義に関するメジャー。クライアントワイヤはそのまま、ディスクは無変更で開いて最初のリライトでフォーマットが昇格し、Rustの顔は一度だけ変わって、以後は凍結されます）と、**2.x → 3.x**（能力のメジャー。すべてが引き継がれました）です。各章は、何が自動でアップグレードされ、何にコード変更が要り、そしてどう戻すのかを明示します。

---

# 3.x → 4.0

4.0は「産業運用に耐える」という宣言です。semverのメジャーは意図的に使いました——長年たまってきた公開APIの負債をすべて払うための、1回きりの破壊の窓です。そしてこれ以降、各面は**凍結されます**。以後の4.xリリースは追加のみです。ワイヤ越しにkevyと話しているなら、4.0はバイナリの差し替えです。kevyのcrateをリンクしているなら、短く機械的な移行を見込んでください——以下の変更にはどれも、1行のルールがあります。

## TL;DR —— バージョンの一覧

| コンポーネント | 3.x時代 | 4.0 | やること |
|---|---|---|---|
| `kevy`（サーバー） | 3.18.x | 4.0.0 | バイナリを差し替え、同じデータディレクトリで再起動する |
| `kevy-embedded` | 3.18.x | 4.0.0 | バージョンを上げ、下のAPI表を適用する |
| `kevy-client` | **1.14.x** | **2.0.0** | バージョンを上げる —— 2つのクライアントは独自のバージョン線を保ち、今回の破壊的変更に対応するのが2.0.0です + API表 |
| `kevy-client-async` | **1.1.x** | **2.0.0** | 同上 |
| `kevy-wasm` / `@goliapkg/kevy`（npm） | — | 4.0.0 | 4.0での新顔 —— [docs/wasm.md](wasm.md) |
| インフラcrate（`kevy-store`、`kevy-rt`、…） | 3.18.x | 4.0.0 | ワークスペースのバージョンに追従する |

`kevy-client`と`kevy-client-async`はワークスペースのバージョン線に**乗っていません**。出荷されるのは**2.0.0**であり、4.0.0ではありません。`cargo add kevy-client`が2.xを解決するのは、それが最新版だからです（古い版ではありません）。

## 自動で互換なもの

**ワイヤプロトコル。** RESPは変わっておらず、verbのエイリアス（`SLAVEOF`、`HMSET`、…）もすべて保たれています。Redisのクライアント、スクリプト、`redis-cli`のセッションは以前どおり動きます。valkey 9.1に対する応答パリティのスイートは、今もCIのゲートです。

**レプリケーションワイヤ（kevy ↔ kevy）——ひとつのクリーンな断絶。** 内部のレプリケーションハンドシェイクはfeed generationを運ぶようになりました（`REPLICATE FROM <gen> <offset> ID <id>` / `+ACK <gen> <offset>`）——プライマリのuncleanな再起動をまたいでもoffset再開要求を安全にする、あのフェンスです。4.0のレプリカは3.xのプライマリとハンドシェイクできず、逆も同様です。レプリケーションペアの両端は同じ窓で揃えて上げてください（レプリカが先、プライマリが後、がダウンタイム最小の順——新しいプライマリが上がるとレプリカは再接続してフル同期します）。これはkevyの内部プロトコルだけの話で、Redisクライアントに面するものは何も変わっていません。

**スナップショットとAOF。** 4.0のバイナリは、3.x（および2.x）のスナップショット形式をすべて読み、3.xのAOFを変更なしにリプレイします——3.xのデータディレクトリは移行作業ゼロで開きます。4.0で新しいのはAOFの*レコードフォーマット*（`KEVYAOF2`：長さ接頭辞 + CRC32Cチェックサム付きレコード——[persistence.md](persistence.md)参照）で、昇格はファイル単位で遅延的、かつ一方向です：

- 既存の3.x（`KEVYAOF1`）ファイルへの追記はv1のまま——バイナリを替えた初日、ファイルは3.18とバイト互換で、ダウングレードはまだバイナリを戻すだけです。
- 新しいファイルと、**最初のリライト**（自動リライトまたは`BGREWRITEAOF`）の出力はv2——3.xには読めません。最初のリライト以降、同じデータディレクトリで3.18へ戻る道はありません（クライアント経由でキー空間をリプレイし直す以外には）。

カナリア期間中に3.18への退路を残したいなら：期間中は自動リライトを止め——サーバーは`auto_aof_rewrite_percentage = 0`、組み込みは`Config::with_auto_aof_rewrite_disabled()`（4.1。3つのトリガーノブを1呼び出しで）——先にスナップショットのバックアップを取ってください。カナリアが定着したら戻すこと。CRCの保護はファイルがv2になって初めて効き始めるので、この状態をカナリアに必要な時間より長く続けないでください。4.1からこの窓は推測ではなく**観測可能**です：組み込みは`Store::downgradeable_to_v3()`、サーバーは`INFO persistence`の`aof_format:`。

**Config。** 3.xのconfigキーはすべて受け入れられ、意味も同じです。2つのキーだけ、*より厳格に、あるいはより誠実に*なりました——下の「振る舞いの変更」を参照してください（`notify_keyspace_events`の未知フラグの拒否、`min_replicas_max_lag_ms`の強制）。

**削除されたノブが1つ。** スナップショット/AOFの*ファイル名*のカスタマイズはなくなりました（`kevy-embedded`の`Config::with_snapshot_filename` / `with_aof_filename`ビルダー）。オンディスクのレイアウトは、シャードごとに`dump-{i}.rdb` / `aof-{i}.aof`で固定になりました。デフォルト名で書かれていたディレクトリは——レガシーな単一ファイルのディレクトリも含めて——そのままロードされます。*カスタム*名で書かれたディレクトリだけ、4.0で最初に開く前に、固定の名前へ一度`mv`する必要があります。

## APIの破壊一覧

この節にあるものはすべて、コンパイル時の破壊であり、機械的な修正があります。明示した箇所を除いて、実行時のセマンティクスは変わりません。

### 1. `flush()`のshimを削除

非推奨だったエイリアスはなくなりました。生き残った名前は、自分がやること（ストアを**消し飛ばす**こと）を名乗ります。

| Crate | 削除 | 代わりに呼ぶもの |
|---|---|---|
| `kevy-embedded` | `Store::flush()` | `Store::flushall()` |
| `kevy-client` | `Connection::flush()` | `Connection::flushall()` |
| `kevy-store` | `Store::flush()` | `Store::flushall()` |

### 2. エラーの通貨を1つに：`KevyError`

`kevy-embedded`と`kevy-client`の、失敗しうる公開の面はすべて、`io::Result<T>`ではなく`KevyResult<T>`（`Result<T, KevyError>`）を返すようになりました。この型は`kevy-store`に住み、両方のcrateが再エクスポートしています。

```rust
pub enum KevyError {
    Store(StoreError),      // structured engine errors, no longer
                            // flattened into io::Error strings
    Io(std::io::Error),     // real I/O, preserved via From
    Protocol(String),       // server error replies, wire text intact
    ReadOnly,               // replica write rejection
    InvalidInput(String),   // e.g. URL parse errors
    NotFound(String),
    Unsupported(String),    // e.g. remote-only calls on embedded
    TimedOut,               // e.g. Subscription::recv_timeout
    Closed,                 // stream/bus gone; also terminates
                            // subscriber iterators
}
```

移行はたいてい、戻り値型の注釈を書き換えるだけです——`From<io::Error>`と`From<StoreError>`があるので、`?`はそのまま動き続けます。

```rust
// 3.x
fn warm(conn: &mut Connection) -> std::io::Result<()> {
    conn.set(b"greeting", b"hello")?;
    Ok(())
}

// 4.0
use kevy_client::{Connection, KevyResult};

fn warm(conn: &mut Connection) -> KevyResult<()> {
    conn.set(b"greeting", b"hello")?;
    Ok(())
}
```

エラーを*検査していた*コードは、厳密に得をします——文字列をパースするのではなく、バリアントにマッチさせてください。

| 3.xでのシグナル | 4.0 |
|---|---|
| `io::Error::other("kevy-store: …")`のラッパーテキスト | `KevyError::Store(e)` —— 構造化された`StoreError` |
| レプリカの書き込み拒否：`io::Error::other("READONLY …")` | `KevyError::ReadOnly`（その`Display`は今も`READONLY`で始まります） |
| サーバーの`-ERR …`応答が、不透明な`io::Error`として | `KevyError::Protocol(text)` —— ワイヤのテキストは保存されます |
| `Subscription::recv_timeout`からの`ErrorKind::TimedOut` | `KevyError::TimedOut` |
| 購読ストリームの消失シグナルとしての`ErrorKind::UnexpectedEof` | `KevyError::Closed`。`SubscriberEvents` / `SubscriberMessages`のイテレータは`KevyResult<_>`を返し、これで終端します |
| リモート専用機能に対する`ErrorKind::Unsupported` | `KevyError::Unsupported(msg)` |

**4.1から、`From<KevyError> for io::Error`は存在します**——kind対応づけ（`TimedOut → ErrorKind::TimedOut`、`Closed → ConnectionAborted`、OOM → `OutOfMemory`、……）で、しかも**sourceを保存します**：型つきの`KevyError`は`io::Error`のsourceとして乗り、ダウンキャストで取り出せるので、境界で何も失われません。4.0はこの逆向きの辺を「損失つきダウングレードの復活」だとして意図的に出荷しませんでした。その後、最初の本番移行がこの変換を約280回、`io::Error::other(e)`として手書きしました——それこそが損失つきダウングレードであり、しかもkind対応づけを欠いています。orphanルールにより、このimplを提供できるのはkevyだけです。だからkevyが提供します。`io::Result`の世界に固定された関数は、いまや`?`だけで済みます。

**移行が実際に何から成るか——それを大規模にやった利用者から**：破壊はエラー型がすべてでした——`Store::open`、`Config`、各メソッドの形は不変です。機械的なレシピ：自分が所有する可謬シグネチャは`KevyResult`へ（たいてい注釈だけ。上の例のとおり）、所有していないところは新しい`From`が`?`を`io::Result`へ運びます。コンパイルエラーを1ページずつ追う代わりに、コンパイラをクエリとして走らせてください——`cargo check --message-format=json`をjqで整形した重複除去済みリストが作業一覧で、その長さが見積もりです。もうひとつ、その利用者が回り道して学んだこと：エラー数は**単調ではありません**——バイナリcrateは依存ライブラリがコンパイルできて初めて自分の変換エラーを出すので、ループは数までではなく、不動点（何も変わらないパス）まで回してください。

（`kevy-resp-client`は意図的に`io::Result`の顔を保っています——純粋なトランスポートの石であり、`io::Error`こそがその誠実な通貨だからです。）

### 3. コンストラクタの命名：リソースは`open`、ネットワークは`connect`

種類ごとに1つのverbを、どこでも。ローカルの、ファイルに裏打ちされたものは`open`。ピアを持つものは`connect`。純粋にメモリ上の値は`new`。改名は次のとおりです。

| Crate | 3.x | 4.0 |
|---|---|---|
| `kevy-client` | `Connection::open(url)` | `Connection::connect(url)` |
| `kevy-client` | `Subscriber::open(url, channels)` | `Subscriber::connect_channels(url, channels)` |
| `kevy-client-async` | `AsyncConnection::open(url)` | `AsyncConnection::connect(url)` |
| `kevy-client-async` | `AsyncSubscriber::open(url, channels)` | `AsyncSubscriber::connect_channels(url, channels)` |
| `kevy-resp-client` | `RespClient::from_url(url)` | `RespClient::connect_url(url)` |

すでに準拠していて変更がないもの：`kevy_embedded::Store::open`、`kevy_persist::Aof::open`、`kevy_store::Store::new`、`ClusterClient::connect`、`RwClient::connect`、`Subscriber::connect(url)`、`RespClient::connect(host, port)`。

### 4. `kevy_rt::Runtime`は、位置引数ではなくビルダーで組む

位置引数のコンストラクタはなくなりました。`Runtime`は自分自身のビルダーです。

```rust
// 3.x
let rt = Runtime::new([127, 0, 0, 1], 6004, 4, commands);

// 4.0
let rt = Runtime::builder(commands)
    .bind([127, 0, 0, 1], 6004)
    .shards(4);
```

`builder(commands)`のデフォルトは、bindが`127.0.0.1:6004`、シャード1つ、AOFはオン（`EverySec`）、データディレクトリは`"."`です。`bind` / `shards`は、既存の`with_*`チェーンと同じく`#[must_use]`のセッターです。これは4.0のインスタンス化作業の、目に見える顔でもあります。`Runtime`はもうグローバル状態に触れないため、1つのプロセスで複数の独立したkevyインスタンスを走らせられます。

### 5. `kevy-store`の書き込みは、借用したargvを取る

所有権を渡す形式は削除され、借用する形式（以前の`_borrowed`の双子）が正規の名前を引き継ぎました。

| 3.x（所有） | 4.0（借用、同じ名前） |
|---|---|
| `del(&[Vec<u8>])` / `exists(&[Vec<u8>])` | `del(&[&[u8]])` / `exists(&[&[u8]])` |
| `hset(&[(Vec<u8>, Vec<u8>)])` / `hdel(&[Vec<u8>])` / `hmget(&[Vec<u8>])` | `hset(&[(&[u8], &[u8])])` / `hdel(&[&[u8]])` / `hmget(&[&[u8]])` |
| `sadd` / `srem` / `lpush` / `rpush` / `zrem` `(&[Vec<u8>])` | 名前は同じで、`(&[&[u8]])` |
| `zadd(&[(f64, Vec<u8>)])` | `zadd(&[(f64, &[u8])])` |
| `zadd_flags_borrowed(…)` | `zadd_flags(…)`に改名 |

```rust
// 3.x
store.del(&[b"k1".to_vec(), b"k2".to_vec()]);

// 4.0 — pass slices; no allocation
store.del(&[b"k1".as_slice(), b"k2".as_slice()]);
```

これはAPI変更の衣を着た性能修正です。`kevy-embedded`のファサードは借用したargvをそのまま素通しするようになり、組み込みのすべての書き込みパスにあった、呼び出しごとの`to_vec()`コピーが消えました。

### 6. `Commands`トレイト + `Route`（独自コマンドセットを持つ組み込み側）

自分で`impl kevy_rt::Commands`を書いている場合にだけ関係します。

- `dispatch_resp3`（Vecを返す形式）を削除 —— `dispatch_into_resp3`をオーバーライドしてください。
- `wake_idx`メソッドを削除 —— 自分の`resolve()`の中で`ResolvedCmd::wake_idx`を埋めてください。フィールド自体は変わっていません。
- `extension_reduce_v3`と、古い2引数の`extension_reduce`を`extension_reduce(argv, chunks, proto) -> ExtensionReduced`に統合。`ExtensionReduced::Reply(bytes)`を返すか、古いNUL接頭のin-band継続フレームの代わりに`ExtensionReduced::Continue(argv2)`を返してください。
- `Route::{MGet, SInter, SUnion, SDiff, ZInterCard}`を`Route::Gather(MultiOp)`に、`Route::{Keys, Scan, RandomKey}`を`Route::Keyspace(KeyShape, Option<Vec<u8>>)`に畳み込み。`MultiOp`と`KeyShape`は新たにpublicになりました。
- `Commands::on_replication_view`のreplicasエントリが`(String, Ipv4Addr, u16, u64, Option<ReplicaAck>)`になりました —— 先頭にレプリカidの`String`、続いてピアの`(Ipv4Addr, u16)`、送信済みオフセット、そして`ReplicaAck { acked_offset, ack_age_ms }`（素のackされたオフセットを置き換えます）です。分解して受けてください。

## 振る舞いの変更（コード変更は不要、運用からは見える）

- **`-LOADING`が本物になりました。** レプリカがフル再同期のスナップショットを飲み込んでいるあいだ、読み出しは、半分だけ置き換わったデータセットを返すのではなく`-LOADING`を返します。`PING`、`INFO`、`HELLO`は答えられるままです（ヘルスチェックは動き続けます）。Redisが免除しているverbと一致します。Redis向けに書かれた「`-LOADING`ならリトライ」のループは、そのまま正しく動きます。
- **`notify_keyspace_events`が未知のフラグ文字を拒否します。** 黙って無視するのではなく、configのパース時に拒否するようになりました——そしてフラグ集合には、本物の`x`（expired）、`e`（evicted）、`n`（new-key）イベントが加わりました。これまでタイポを紛れ込ませていたconfigは、これからは大きな音を立てて失敗します。フラグ文字列を直してください。
- **`min-replicas-to-write`は、生きたACKだけを数えます。** `min_replicas_max_lag_ms`というキーは3.xにも存在しましたが、いまや強制されます。最後のACKが窓より古いレプリカは、書き込みゲートを満たさなくなりました。*停滞した*レプリカに頼って書き込みを流し続けていたデプロイには`-NOREPLICAS`が見えるようになります——それこそが、このキーがずっと約束していたセマンティクスです。
- **`CLIENT`の面が真実を語ります。** `CLIENT LIST`は本物のコネクションテーブルであり（getpeernameに裏打ちされたアドレス、グローバルに一意なid）、`CLIENT KILL`は本当にkillし（ブロック中のコネクションも含めて）、`CLIENT SETNAME`は定着し、`INFO`の`connected_clients`は生きたゲージです。
- **`SHUTDOWN`は行儀よくdrainします。** 処理中の応答を出し切り、終了前にAOFが最後のfsyncを受けます。素のSIGTERMがsyncされないまま残していた、everysecのテール窓が閉じます。
- **`ROLE` / `INFO replication`がシャードをまたいで集約します。** レプリカごとの識別情報つきです（`ip:port`と、レプリカごとの真のオフセット）。サーバーごとに要約行が1つ、と仮定していたパーサはそのまま動きます。レプリカごとの行が、より豊かになります。

## フィーチャシステム（4.0での新顔）

`kevy-embedded`はフィーチャで階層化され、小さなターゲットが使う分だけを支払うようになりました。デフォルトは今までどおり全部入りです。

| フィーチャ | 何が加わるか | 引き込むもの |
|---|---|---|
| `core` | KV + TTL + pubsub + atomic/pipeline | （なし） |
| `persist` | スナップショット + AOF | `kevy-persist` |
| `index` | 宣言型インデックス + ビュー | `kevy-index` |
| `text` | 全文（BM25）のセグメント | `index`、`kevy-text` |
| `vector` | HNSWのANNセグメント | `index`、`kevy-vector` |
| `replicate` | レプリケーション + CDCフィード | `persist`、`kevy-replicate` |
| `listener` | 読み取り専用のRESPリスナー | （なし） |

`core`階層はmuslターゲット向けにクロスコンパイルでき、強制された予算を持ちます（バイナリ700 KB以下、空ストアのRSS 2 MB以下）。加えて、5つの基盤crateは`no_std`でもビルドされます。[docs/iot.md](iot.md)を参照してください。サイズのスペクトルの反対側では、その同じ組み込みコアが`@goliapkg/kevy`としてブラウザ上でも走ります——[docs/wasm.md](wasm.md)を参照してください。

## 4.0 → 3.18のダウングレード

バイナリを戻すだけです。スナップショットとAOFの形式は共有されています。唯一の縁は、新しい通知フラグ（`x`/`e`/`n`）を使っているconfigファイルです。3.18でもパースは通りますが、そこでイベントが発火することはありません。

---

# 2.x → 3.x

kevy 3.xは2.xのスーパーセットです。2.xのワークロードはすべてそのまま動き、アップグレードはサーバーならバイナリの差し替え、組み込みユーザーなら依存のバージョン上げです。この章は、何が自動で引き継がれ、何が名前や数字を変え、そしてどの方向にだけ注意が要るのか（2.xへのダウングレード）を明示します。

## TL;DR —— バージョンの一覧

| コンポーネント | 2.x時代 | 3.x（最終：3.18.x） | やること |
|---|---|---|---|
| `kevy`（サーバー） | 2.0.x | 3.18.x | バイナリを差し替え、同じデータディレクトリで再起動する |
| `kevy-embedded` | **1.x**（1.4〜1.16） | **3.x** | 依存を上げる —— ワークスペース全体が1つのバージョンに統一されたv3.0.0で、1.xのラインは終わりました |
| `kevy-client` | 1.12.x | 1.13〜1.14 | 上げるだけ。APIは不変 |
| `kevy-client-async` | 1.0.x | 1.1.x | 上げるだけ。APIは不変 |
| `kevy-cli` | 未公開 | 3.x | `cargo install kevy-cli` —— 移行ツールチェーン一式を運ぶようになりました |
| インフラcrate（`kevy-store`、`kevy-rt`、…） | 2.0.x | 3.x | ワークスペースのバージョンに追従する |

`kevy-embedded`の1.xから3.xへの跳躍は、**バージョンラインの統一であって、APIの書き直しではありません**。1.16の面は3.xに含まれています。`Cargo.toml`に`kevy-embedded = "1"`と書いてあるなら`"3"`に変えて再ビルドしてください——あるいは、上の章とともに一気に`"4"`へ進んでください。

## 自動で互換なもの

**ワイヤプロトコル。** RESPは変わっていません。3.xも、CIでvalkey 9.1に対してバイト単位の応答チェックを受け続けています（98コマンド）。既存のRedisクライアント、スクリプト、`redis-cli`のセッションは以前どおり動きます。

**スナップショット。** 3.xのローダは、2.xのスナップショット形式をすべて読みます（`KEVYSNAP`のバージョン2〜5：相対TTLのv2ファイル、絶対TTLのv3、ストリームグループのv4、フィードカーソルのv5）。3.xのサーバーを2.xのデータディレクトリに向ければ、ロードされます。

**AOF。** AOFはverbのログであり、3.xのverb集合は2.xのスーパーセットです——リプレイはそのまま動きます。`appendfsync`のセマンティクスも変わりません。

**Config。** 2.xのconfigキーはすべて受け入れられます。新しいセクション（`[replication] single_source`、`--accept-shards`、…）は追加的であり、デフォルトは2.xの振る舞いを再現します。

## アップグレードの手順

### サーバーのデプロイ

1. 稼働中の2.xサーバーでスナップショットを取り（`SAVE`または通常のバックアップ）、そのコピーを保管してください——理由は下の「ダウングレード」を参照。
2. 2.xを止め、同じフラグとデータディレクトリで3.xのバイナリを起動します。
3. 検証：`DBSIZE`が一致すること。暗号強度の保証が欲しければ、前後で`kevy-cli digest -p <port> <prefix>`を走らせてください——ダイジェストが等しければ、キー空間は同一です。

レプリカのペアをローリングで上げる場合：先にレプリカをアップグレードし、再同期させ、トラフィックをそちらへフェイルオーバーしてから、元プライマリをアップグレードします。（2.xに管理されたフェイルオーバーはないので、2.xからだとこれは通常の手動の入れ替えになります。3.15以降に乗れば、そのフェイルオーバーの手順自体が1つのverbになります——`FAILOVER host port`、[docs/availability.md](availability.md)を参照。）

### 組み込みアプリケーション

1. `Cargo.toml`に`kevy-embedded = "3"`。
2. リビルドします。1.16のAPIはそのまま存在します。新しい能力の面（index/view/text/vector/feed/replication）は、追加のメソッドと`Config`のオプションです。
3. トレイトについて1点。独自の`impl kevy_rt::Commands`を書いていて、かつ`ResolvedCmd`をリテラルで構築している場合に限りますが、v2のアークの途中で2つのフィールドが増えました（`block_hint`、`wake_idx`）。デフォルトの`resolve()`はそれらを埋めます。リテラルのコンストラクタには、2つのフィールドを足してください。
4. 組み込み1.xのアプリが書いたオンディスクのデータは、そのままロードされます（サーバーと同じスナップショット形式です）。

### クライアント

`kevy-client 1.13+` / `kevy-client-async 1.1`はドロップインです。マイナーの上げは、内部のcrateを3.xのワークスペースに固定し直しただけです。汎用のRedisクライアントライブラリは、どちらにせよ影響を受けません。

## 3.xで加わるもの（アップグレードする理由）

hydration付きの宣言型インデックス（`IDX.*`）、名前付きビュー（`VIEW.*`）、書き込み時の集約（GROUP BY / 分散top-K）、辞書不要のCJK全文検索とBM25、HNSWによるベクトルKNN（さらにBM25+KNNのハイブリッド融合）、recovery-point契約を持つCDCフィード（`FEED.*`）、組み込みをプライマリにするレプリケーション、機械可読な契約（`COMMAND DOCS`、生成されるリファレンス、`kevy-mcp` MCPサーバー）、可用性のアーク（レプリケーションのラグの真実、`FAILOVER`、クォーラムのクラッシュ選挙、`WAIT` / `REPL.TOKEN` / `REPL.WAIT`の整合性ラダー——[docs/availability.md](availability.md)）、そして移行ツールチェーン（`kevy-cli import/export/--verify/diff/inspect/digest`）です。[docs/designing-on-kevy.md](designing-on-kevy.md)と[docs/cookbook.md](cookbook.md)から始めてください。性能の領収書は[bench/PERF-LEDGER.md](../../bench/PERF-LEDGER.md)にあります。

これらのどれも、暗黙に有効化されることはありません。2.xのワークロードを載せた3.xサーバーは空のカタログを持ち、空のカタログに対するインデックスフックはperfgateのラチェットに載っています（2.x比でリグレッションなし）。

## ダウングレード（唯一、注意が要る方向）

3.xのサーバーはスナップショット形式v4を**書きます**。CDCフィードのカーソルが存在すればv5です。2.xのバイナリはv4までしか読めません。

- フィードを一度も有効にしていないなら、3.xのスナップショットは2.xでロードされます。
- フィードが有効だった（v5）なら、2.xはそのファイルを拒否します。ダウングレードの道筋：3.xで`kevy-cli export` → まっさらな2.xへ`kevy-cli import`。あるいは、手順1で取ったアップグレード前のバックアップを戻し、その間の差分を受け入れてください。

3.xで導入されたverb（`IDX.*`、`VIEW.*`、`FEED.*`、…）は、当然ながら2.xのバイナリではリプレイされません——それらを使っていたなら、正しいダウングレードはAOFのリプレイではなく、export/importの道です。

## バージョン履歴、各1行

- **3.0.0** —— サービングエンジンの宣言（インデックス、ビュー、FTS、ANN、CDC、オンランプ。ゲートされた11本の列車）。
- **3.8.0** —— 性能のアーク（valkey 9.1とRediSearchに対する実測。素の面で1.6〜3.3倍、ANNは再現率1.000で1.64倍の先行、FTSの単一の一般的な語で93倍。組み込みをプライマリにするレプリケーション）。3.0.0と3.8.0のあいだにリリースは切られていません。3.8.0はv3.1〜v3.8の列車を含みます。
- **3.17.0** —— 可用性のリリース。AIネイティブなサービングの面（機械可読なverb契約、生成されるドキュメント、`kevy-mcp`、ハイブリッド検索）と、可用性のアーク（レプリケーションのハートビート/ACKの真実、`FAILOVER` + クォーラムのクラッシュ選挙、整合性ラダー、CIの契約ゲート）。v3.9〜v3.17の列車を含みます。あいだにリリースは切られていません。
- **3.17.1〜3.17.4** —— メンテナンス。`luna-core` Luaランタイムのバージョン上げ、ドキュメント/移行の波、初期採用者のフィードバック（`kevy-cli --embed`）、そしてドキュメント/i18nの磨き上げの波。
- **3.18.0** —— 構造のリリース。LOCの負債をゼロにし、その上限をCIで強制。さらに6つの石をfuzz（初日の収穫：実バグ4件を修正）。miri/pedantic/missing-docsの一掃、Rust 1.97.0。
