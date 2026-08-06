# RDS→kevyモデリング・クックブック

リレーショナルなデータモデルをkevyへ移すためのレシピ集です。以下のレシピはすべて出荷済みのプリミティブだけを使います——ロードマップ上の機能も「近日公開」もありません。各レシピは、置き換える対象のRDS概念と、それを担うkevyのパターンを名指しで示します。

すべてのレシピに共通する設計姿勢は、**スキーマではなくアクセスパスをモデリングする**ことです。RDSではその決定をクエリプランナーに先送りできますが、kevyはあなた自身に宣言させます——その見返りが、サービング時のマイクロ秒単位のページ応答です（実測値は`bench/VALIDATION-LEDGER.md`にあります）。

各コマンドブロックは、まっさらなローカルkevy（`kevy --port 6004`。レシピ11〜14、16、20はさらに`kevy.toml`に`[feed] enabled = true`が必要です——docs/cdc.md参照）に対してそのまま実行できます。`bench/cookbook_smoke.sh`が下記のすべての`kevy-cli`行を使い捨てサーバーに対して実行するので、ブロックの内容は常に正直に保たれます。

## 1. テーブルと行

**SQL相当：**`CREATE TABLE` + `SELECT col FROM t WHERE id = ?`——[マトリクス：テーブル・行・カラム](../rds-workloads.md#tables-rows-columns)。

行は、型を表すプレフィックスの下のハッシュです。

```console
kevy-cli -p 6004 HSET user:42 name ada email ada@example.com age 36
kevy-cli -p 6004 HGET user:42 name
kevy-cli -p 6004 HGET user:42 phone    # NULL = absent field: already answers (nil)
```

- テーブル→キープレフィックス（`user:`）。カラム→ハッシュフィールド。主キー→キーそのもの。
- **NULL＝フィールドの不在。**番兵文字列を格納してはいけません。存在しないフィールドへの`HGET`はすでにnilを返しますし、インデックス仕様はフィールドの欠けた行を「除外された行」として扱います（`IDX.VERIFY`のカウントで確認できます）。
- カラムの型はあなたの管轄です。kevyが格納するのはバイト列です。型が意味を持つ場所——インデックス作成時（`TYPE i64|f64|str|vector`）——で宣言してください。型強制の失敗はカウントされ、黙ってインデックスされることはありません。

## 2. 1対多、多対多

**SQL相当：**外部キーカラム＋中間テーブル。`SELECT … FROM orders WHERE user_id = ?`——[マトリクス：JOIN](../rds-workloads.md#join)。

リンクキーが関係を担います。片側ごとに1つのセットです。

```console
kevy-cli -p 6004 HSET order:1001 user_id 42 total 1999 status shipped
kevy-cli -p 6004 HSET order:1002 user_id 42 total 550 status pending
kevy-cli -p 6004 SADD user:42:orders 1001 1002       # 1-N: member = order id
kevy-cli -p 6004 RPUSH order:1001:items sku-7 sku-9
kevy-cli -p 6004 SADD tag:urgent:orders 1001         # N-M: one set per side
kevy-cli -p 6004 SADD order:1001:tags urgent
```

あるいはリンクキーを一切使わない手もあります。外部キーを行の中に置き（上の`user_id`）、インデックスを宣言する——`IDX.QUERY … EQ 42`がこの世界の`SELECT … WHERE user_id = 42`で、1ホップでハイドレートされます。

```console
kevy-cli -p 6004 IDX.CREATE order_user ON PREFIX order: FIELD user_id TYPE i64 KIND range
kevy-cli -p 6004 IDX.QUERY order_user EQ 42 FIELDS total status
```

## 3. シーケンス

**SQL相当：**`AUTO_INCREMENT` / `CREATE SEQUENCE` + `nextval()`——[マトリクス：主キー・UNIQUE・AUTO_INCREMENT](../rds-workloads.md#primary-key-unique-auto_increment)。

```console
kevy-cli -p 6004 INCR seq:order          # one id
kevy-cli -p 6004 INCRBY seq:order 100    # block allocation: hand out 100 ids
                                         # from app memory, refill when dry
```

ブロック割り当てが高スループット形です。クラッシュ時に欠番が出るのは、PostgreSQLのシーケンスと同じ契約です。

## 4. 楽観ロック（行バージョン）

**SQL相当：**`UPDATE t SET …, version = v+1 WHERE id = ? AND version = v`（バージョンカラムCAS）——[マトリクス：トランザクション](../rds-workloads.md#transactions)。

サーバー側はWATCH/MULTI——CASループです。トランザクションはコネクションスコープなので、1つのREPLセッション内で実行します（ここではヒアドキュメントで流し込みます）。

```bash
kevy-cli -p 6004 HSET user:42 balance 100 version 7
kevy-cli -p 6004 <<'TXN'
WATCH user:42
HGET user:42 version
MULTI
HSET user:42 balance 90 version 8
EXEC
TXN
```

`WATCH`の後に誰かが`user:42`に触れていた場合、`EXEC`はnilを返します——レースに負けたということなので、読み直してリトライしてください。

組み込み側では、読んで・判断して・書くの一連を1つの`atomic()`ブロック内で実行します——シャードロックのおかげで、リトライループなしに分岐がレースフリーになります。

## 5. CHECK制約と複数キー不変条件

**SQL相当：**`CHECK (balance >= 0)`＋トリガー維持の監査行——[マトリクス：制約とトリガー](../rds-workloads.md#constraints-and-triggers)。

RDSは`CHECK (balance >= 0)`をエンジン内で実行します。kevyでの置き換えは**アトミックブロック内の読み取り**です。不変条件の評価はアプリが行い、その判断と書き込みコミットが一体であることをエンジンが保証します。

```rust
// embedded — debit that must not overdraw, plus an audit row:
store.atomic(b"acct:7", |ctx| {
    let bal: i64 = parse(ctx.hget(b"acct:7", b"balance")?);
    if bal < amount { return Err(Overdraw); }
    ctx.hset(b"acct:7", &[(b"balance", &(bal - amount))])?;
    ctx.rpush(b"acct:7:ledger", &[entry])?;
    Ok(())
})
```

シャードをまたぐ不変条件には`atomic_all_shards`（決定的ロック順序、文書化されたデッドロック免除）があります。使いどころは控えめに——これは直列化可能トランザクションという名のハンマーであり、大半の不変条件は設計上1つのキープレフィックスの下に収まるものです。

## 6. 冪等性キー

**SQL相当：**`UNIQUE INDEX` + `INSERT … ON CONFLICT DO NOTHING`——[マトリクス：主キー・UNIQUE・AUTO_INCREMENT](../rds-workloads.md#primary-key-unique-auto_increment)。

```console
kevy-cli -p 6004 HSET req:9001 idem_key pay-2026-07-04-a77 amount 1999
kevy-cli -p 6004 IDX.CREATE req_idem ON PREFIX req: FIELD idem_key TYPE str KIND unique
kevy-cli -p 6004 IDX.QUERY req_idem EQ pay-2026-07-04-a77   # duplicates are visible as multi-hit reads
kevy-cli -p 6004 IDX.VERIFY req_idem                        # ...and counted here
kevy-cli -p 6004 SET idem:pay-2026-07-04-a77 1 NX PX 86400000
```

行を書いてからクエリします——重複は*可視*です（uniqueは書き込みを拒否する代わりにVERIFYで重複を数える、宣言的なフェンスであって書き込みゲートではありません）。ハードなゲートが必要なら、処理の前に`SET … NX PX`形を使います。NXがアトミックな占有宣言で、TTLが保持ウィンドウです。

## 7. ソフトデリート

**SQL相当：**`deleted`フラグカラム＋部分インデックス／ビュー`WHERE deleted = 0`——[マトリクス：VIEW](../rds-workloads.md#view)。

消さずにフラグを立てます。

```console
kevy-cli -p 6004 HSET user:42 deleted 0 age 36
kevy-cli -p 6004 HSET user:43 deleted 1 age 51
kevy-cli -p 6004 IDX.CREATE user_live ON PREFIX user: FIELD deleted TYPE i64 KIND range
kevy-cli -p 6004 IDX.QUERY user_live EQ 0 LIMIT 100    # live rows only
```

ビューを使えばフィルタを恒久的に畳み込めます——呼び出し側が条件を毎回書き直す必要はなくなります。

```console
kevy-cli -p 6004 IDX.CREATE user_age ON PREFIX user: FIELD age TYPE i64 KIND range
kevy-cli -p 6004 VIEW.CREATE live_users QUERY '(' AND user_live EQ 0 user_age RANGE 18 200 ')' ORDER BY user_age
kevy-cli -p 6004 VIEW.QUERY live_users LIMIT 10
```

## 8. 複合順序付け（ORDER BY a, b）

**SQL相当：**複合インデックスでの`ORDER BY a, b`——[マトリクス：ORDER BY / LIMIT / OFFSET](../rds-workloads.md#order-by--limit--offset)。

複合キーを書き込み時に1つのインデックス対象フィールドへエンコードします。有界な整数`b`なら`score = a * 1_000_000 + b`、辞書順の複合ならゼロ埋め文字列フィールドです——インデックスは1本、ORDER BYも1つ。書き込みフックが他のフィールドと同様に維持します。

```console
kevy-cli -p 6004 HSET evt:1 ord '2026-07-04|000042'
kevy-cli -p 6004 HSET evt:2 ord '2026-07-04|000007'
kevy-cli -p 6004 HSET evt:3 ord '2026-07-05|000001'
kevy-cli -p 6004 IDX.CREATE evt_ord ON PREFIX evt: FIELD ord TYPE str KIND range
kevy-cli -p 6004 IDX.QUERY evt_ord RANGE '2026-07-04|000000' '2026-07-04|999999' LIMIT 100
```

## 9. JSONB

**SQL相当：**生成カラムインデックス付きのJSON/JSONBカラム——[マトリクス：型システム](../rds-workloads.md#type-system)。

ハッシュフィールドへ平坦化します。`profile.city`→フィールド`profile.city`。フィールド単位の読み書き、フィールドTTL（HEXPIRE）、インデックス可能性はそのまま残ります——JSONBが与えてくれたもののうちJSONパスクエリだけが**恒久的に対象外**です（クエリエンジンへの坂道。docs/designing-on-kevy.mdのREFUSEDテーブル参照）。

```console
kevy-cli -p 6004 HSET user:7 profile.city tokyo profile.plan pro
kevy-cli -p 6004 HGET user:7 profile.city
kevy-cli -p 6004 HEXPIRE user:7 3600 FIELDS 1 profile.plan   # per-field TTL survives the flattening
```

誰もインデックスしない深いネストのブロブは、シリアライズ済みの1フィールドのままで構いません。パスが意味を持った瞬間に、フィールドへ昇格させてください。

## 10. カスケード削除／外部キー

**SQL相当：**`FOREIGN KEY … ON DELETE CASCADE`——[マトリクス：制約とトリガー](../rds-workloads.md#constraints-and-triggers)。

カスケードはアプリのパターンであって、エンジンの魔法では決してありません。

- 同期・小さな影響範囲：1つのアトミックブロック内で削除（`ctx.del(row)`、`ctx.srem(parent_link, id)`）。
- 一括・プレフィックス形：`delete-prefix`——レート制限つき、再開可能。
- 非同期：CDCコンシューマ（`PREFIX`付き`FEED.READ`）が親の削除に反応して子を掃除——コミット後・疎結合・リプレイ可能な、トリガーの置き換えです。

```console
kevy-cli -p 6004 HSET order:1001 user_id 42
kevy-cli -p 6004 RPUSH order:1001:items sku-7 sku-9
kevy-cli -p 6004 SADD order:1001:tags urgent
kevy-cli delete-prefix -p 6004 --rate 5000 order:1001:   # children gone, parent row stays
```

## 11. 不要になるアウトボックス

**SQL相当：**トランザクショナル・アウトボックステーブル＋リレーワーカー——[マトリクス：CDC](../rds-workloads.md#cdc)。

トランザクショナル・アウトボックスというパターンは、RDSのコミットとメッセージバスへのpublishをアトミックにできないから存在します。kevyでは**フィードがアウトボックス**です。コミットされた各書き込みは、すでに`(generation, offset)`カーソル位置の変更フレームであり、at-least-once配送・プレフィックスフィルタ可能です（docs/cdc.md）。`FEED.READ`を消費してください。二本目のジャーナルを組んではいけません。

```console
# needs [feed] enabled = true in kevy.toml (docs/cdc.md)
kevy-cli -p 6004 HSET order:9001 status paid
kevy-cli -p 6004 FEED.SHARDS
kevy-cli -p 6004 FEED.TAIL 0                             # a fresh consumer's starting cursor
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 10 PREFIX order:  # gen 1 = a fresh data dir's first generation
```

## 12. 監査履歴

**SQL相当：**トリガー維持の監査／履歴テーブル（またはbinlog考古学）——[マトリクス：CDC](../rds-workloads.md#cdc)。

CDCの保持期間こそが監査ログです。フレームはコミット順に、適用された効果のargvを運びます。コンプライアンス上負っているウィンドウに合わせてフィードのバックログをサイズし、カーソルコンシューマでコールドストレージへエクスポートします。特定時点の再構築には、スナップショットをリストアして`(gen, offset)`リカバリポイントまでリプレイします（docs/persistence.md）。

```console
kevy-cli -p 6004 HSET acct:7 balance 100
kevy-cli -p 6004 HSET acct:7 balance 90
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 100 PREFIX acct:   # who set what, in commit order
```

## 13. ロールバックウィンドウ（逆方向ミラー）

**SQL相当：**カットオーバー中の旧プライマリへの逆レプリケーション——[移行プレイブックのフェーズ5](migration.md)。

カットオーバー中は、kevyへの書き込みを旧RDSへ書き戻すCDCコンシューマ（`FEED.READ`→UPDATE文）を走らせます。こうしておけばロールバック計画は「アプリの向き先を戻す」であって、「データを逆移行する」ではなくなります。確信が固まったらミラーを退役させます。`kevy-cli diff`（プレフィックスごとのダイジェスト）が確信の計器です。

```console
kevy-cli -p 6004 HSET user:42 name ada
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 10 PREFIX user:   # the mirror consumer's read loop
kevy-cli diff 127.0.0.1:6004 127.0.0.1:6004 user:        # digests match: safe form of the check
kevy-cli diff old-rds-mirror.internal:6379 127.0.0.1:6004 user:   # needs-external
```

## 14. 分析エクスポート

**SQL相当：**ウェアハウスへ流すETLジョブ／binlogタップ——[マトリクス：CDC](../rds-workloads.md#cdc)。

サービングと分析はエンジンを共有しません。エクスポートのパターンは次の通りです。

- `export`——論理エクスポート。再開可能で、RESPが通じる場所ならどこへでもロードできます。
- CDC→ウェアハウス：カーソルコンシューマがOLAPストアへinsertをストリーミング。まさにCDC-to-Kafkaの形です。
- 読み取り専用リスナー（`docs/embedded-listener.md`）：組み込みアプリからのアドホックな取り出しに。

```console
kevy-cli -p 6004 HSET order:1001 user_id 42 total 1999
kevy-cli export -p 6004 --prefix order: /tmp/orders.resp
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 100 PREFIX order:   # the CDC-to-warehouse read loop
```

## 15. ロード順序（インデックス後回しの規則）

**SQL相当：**`LOAD DATA`が先、`CREATE INDEX`が後(バルクロードの規律)——[マトリクス：セカンダリインデックスDDL](../rds-workloads.md#secondary-index-ddl)。

バルクロードが**先**、インデックス／ビューの宣言は**後**です。バックフィルは既存の行から100万行あたり約7秒で構築します——インポートする行ごとに書き込みフックのコストを払うより桁違いに安上がりです（docs/migration.md）。

```console
kevy-cli -p 6004 HSET item:1 price 10
kevy-cli -p 6004 HSET item:2 price 25
kevy-cli -p 6004 HSET item:3 price 7
kevy-cli export -p 6004 --prefix item: /tmp/items.resp
kevy-cli import -p 6004 /tmp/items.resp   # bulk load FIRST: no index write hook to pay
kevy-cli -p 6004 IDX.CREATE item_price ON PREFIX item: FIELD price TYPE i64 KIND range   # declare AFTER: backfill
kevy-cli -p 6004 IDX.QUERY item_price RANGE 0 100 LIMIT 10
```

---

続く3つのレシピはワークロードを入れ替えます。置き換える対象はRDSではなく、AIエージェントのメモリスタックです。新しいものは何も要りません——セッション状態も、エピソード記憶も、RAG検索も、キープレフィックスを着替えただけの同じアクセスパスパターンです。

## 16. TTL付きセッションコンテキスト

**SQL相当：**セッションテーブル＋期限切れ掃除のcronジョブ——[マトリクス：運用上の差分](../rds-workloads.md#sizing-and-operational-deltas)。

エージェントの作業コンテキストは、リース付きの1行です。コンパクション済みの会話はハッシュに住み、`EXPIRE`がアイドル退去ポリシーです（ターンごとに更新——スライディングウィンドウ）。そして「ターン7の時点でエージェントは何を知っていたか」と聞かれたときにリプレイする監査証跡が、フィードです。

```console
# needs [feed] enabled = true in kevy.toml (docs/cdc.md)
kevy-cli -p 6004 HSET session:a7 user 42 turns 6 messages 'wants refund for order 1001; tone calm' last_tool order_lookup
kevy-cli -p 6004 EXPIRE session:a7 3600
kevy-cli -p 6004 HSET session:a7 turns 7 messages 'refund approved; awaiting confirmation'
kevy-cli -p 6004 EXPIRE session:a7 3600                       # renew the lease on every turn
kevy-cli -p 6004 FEED.TAIL 0                                  # audit cursor: where the log ends now
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 100 PREFIX session:    # gen 1 = a fresh data dir's first generation
```

`messages`フィールドの中身は、あなたのコンパクションステップが生成する要約なら何でも構いません。書き換えは`HSET`1回で、しかもすべての改訂はすでにコミット順の変更フレームになっています——多くのエージェントフレームワークが後付けする「会話履歴」テーブルは、レシピ12の監査ログとしてタダで手に入ります。

## 17. エピソード記憶（時間×意味）

**SQL相当：**`WHERE ts BETWEEN …`＋pgvectorの`ORDER BY embedding <=> ? LIMIT k`——[マトリクス：SELECT](../rds-workloads.md#select)。

エピソード記憶は、同じ行たちに対して2つの質問に答えます。*最近何が起きたか*（時間）と、*これに似ているものは何か*（意味）です。プレフィックスは1つ、質問ごとにインデックスを1本——`DIM 8`はデモを読みやすくするためで、実際の埋め込みは768次元以上をf32-LEブロブとして送ります。下の`csv:`デバッグ形式は、ベクトルを受け付けるすべての場所で使えます（格納フィールドもクエリベクトルも同じパーサーを通ります——docs/vector-search.md）。

```console
kevy-cli -p 6004 HSET mem:1 ts 1783200000 kind obs what 'user prefers dark roast' v csv:0.9,0.1,0,0,0,0,0,0
kevy-cli -p 6004 HSET mem:2 ts 1783203600 kind obs what 'user asked about decaf' v csv:0.8,0.3,0.1,0,0,0,0,0
kevy-cli -p 6004 HSET mem:3 ts 1783207200 kind reflection what 'coffee questions cluster in the morning' v csv:0,0.2,0.9,0.1,0,0,0,0
kevy-cli -p 6004 IDX.CREATE mem_ts ON PREFIX mem: FIELD ts TYPE i64 KIND range
kevy-cli -p 6004 IDX.CREATE mem_kind ON PREFIX mem: FIELD kind TYPE str KIND range
kevy-cli -p 6004 IDX.CREATE mem_ann ON PREFIX mem: FIELD v TYPE vector KIND ann DIM 8
kevy-cli -p 6004 IDX.QUERY mem_ts RANGE 1783203000 1783210000 LIMIT 10 FIELDS what      # recent memories
kevy-cli -p 6004 IDX.QUERY mem_ann KNN csv:0.85,0.2,0,0,0,0,0,0 LIMIT 2 FIELDS what ts  # similar memories
kevy-cli -p 6004 IDX.QUERY COMPOSE AND mem_ts RANGE 1783203000 1783210000 mem_kind EQ reflection LIMIT 10 FIELDS what
```

`COMPOSE AND`はスカラーの脚（`RANGE`/`EQ`）を連言します——ここでは「この時間窓の中で、かつreflectionであるもの」。*窓の中で似ているもの*については、意図的にKNNの脚がありません（グラフ探索の内側でのフィルタリングはクエリエンジンへの坂道であり、REFUSEDです）。`LIMIT`に余裕を持たせてKNNを実行し、上のように`FIELDS`で`ts`をハイドレートして、窓の外のヒットをクライアント側で捨ててください。

## 18. ハイブリッド検索のRAGチャンク

**SQL相当：**tsvector全文検索＋pgvector KNNのアプリ側融合——[マトリクス：SELECT](../rds-workloads.md#select)。

チャンクは2つの検索面——テキストとその埋め込み——を併せ持つ行なので、1回の書き込みが両方のインデックスを維持します。

```console
kevy-cli -p 6004 HSET chunk:1 doc kevy-guide seq 1 body 'rows are hashes under a typed key prefix' v csv:1,0,0,0,0,0,0,0
kevy-cli -p 6004 HSET chunk:2 doc kevy-guide seq 2 body 'indexes are declared once and maintained by the write hook' v csv:0,1,0,0,0,0,0,0
kevy-cli -p 6004 HSET chunk:3 doc kevy-guide seq 3 body 'the feed streams every committed write as a change frame' v csv:0,0,1,0,0,0,0,0
kevy-cli -p 6004 IDX.CREATE chunk_text ON PREFIX chunk: FIELD body TYPE str KIND text
kevy-cli -p 6004 IDX.CREATE chunk_ann ON PREFIX chunk: FIELD v TYPE vector KIND ann DIM 8
kevy-cli -p 6004 IDX.QUERY HYBRID chunk_text MATCH 'typed key prefix' chunk_ann KNN csv:0.9,0.1,0.1,0,0,0,0,0 LIMIT 2 FIELDS body
kevy-cli -p 6004 IDX.QUERY HYBRID chunk_text MATCH 'change frame' chunk_ann KNN csv:0,0.1,0.9,0,0,0,0,0 LIMIT 2 RRFK 20 FIELDS body
```

`HYBRID`は両方の脚をサーバー側で実行し、**Reciprocal Rank Fusion**で融合します。各キーはBM25リストとKNNリストにわたって`Σ 1/(k + rank)`のスコアを得ます——ランクのみを使うので、性質の異なる2つのスコア尺度を正規化する必要がなく、*両方*の脚で上位に来るチャンクが片方だけで首位のチャンクに勝ちます。`RRFK`がそのk（デフォルト60）です。各脚のトップヒットを信頼していて、そこでの一致を支配的にしたいなら下げ、両リストのより深いところまで見た合意へ融合を平らにしたいなら上げてください。

---

最後の2つのレシピは、ラックの外へ出ます。エッジノードの上のkevy——同じサーバーバイナリ、あるいは`core`ティアまで絞り込んで655 KBにした`kevy-embedded`（[docs/iot.md](iot.md)）——は同じverbを話すので、パターンはデータセンターからセンサーゲートウェイまでそのまま持ち運べます。

## 19. センサーキャッシュ（最新値＋生存リース）

**SQL相当：**`readings_latest`アップサートテーブル＋鮮度チェックのcron——[マトリクス：運用上の差分](../rds-workloads.md#sizing-and-operational-deltas)。

各センサーの現在値は1つの行で、TTLが生存契約です。報告が止まったセンサーはキャッシュから期限切れで消えます——**不在こそがオフラインの信号**であり、書くべき掃除ジョブはありません。

```console
kevy-cli -p 6004 HSET sensor:t1 val 21.5 unit C ts 1783200000
kevy-cli -p 6004 EXPIRE sensor:t1 90
kevy-cli -p 6004 HSET sensor:t1 val 21.7 unit C ts 1783200030
kevy-cli -p 6004 EXPIRE sensor:t1 90      # every report renews the lease
kevy-cli -p 6004 EXISTS sensor:t1         # 1 = reporting, 0 = gone dark
```

リースの長さはアラーム許容度に合わせます（ここでは90秒＝30秒間隔の報告を3回落としたらオフライン）。ポーリングの代わりにセンサーの沈黙へ*反応*したいなら、`x`（expired）クラスを含むkeyspace notificationsを有効化して期限切れイベントを購読してください——同じ契約のプッシュ形です（docs/pubsub.md）。

直近ウィンドウは、ハードキャップ付きのストリームです——`MAXLEN ~`がノードのメモリを稼働時間に関係なく有界に保ちます。何か月も動き続けるエッジ機ではこれこそが大事な不変条件です。

```console
kevy-cli -p 6004 XADD sensor:t1:log MAXLEN '~' 1000 '*' val 21.5
kevy-cli -p 6004 XADD sensor:t1:log MAXLEN '~' 1000 '*' val 21.7
kevy-cli -p 6004 XLEN sensor:t1:log
kevy-cli -p 6004 XRANGE sensor:t1:log - + COUNT 10
```

組み込み形：ゲートウェイプロセスの中から型付きAPIで同じverbを使います——`store.hset(…)`／`store.expire(…)`／`store.xadd(…)`——ソケットは一切ありません。このレシピが使うものはすべて`core`フィーチャーティアに収まっています（docs/iot.md）。

## 20. エッジ集計（書き込み時GROUP BY＋アップリンク）

**SQL相当：**ダッシュボード更新のたびに再実行される`SELECT zone, COUNT(*), SUM(w) … GROUP BY zone`——[マトリクス：GROUP BYと集計](../rds-workloads.md#group-by-and-aggregates)。

エッジノードはローカルで要約し、要約だけを送ります——生の読み取り値はアップリンクに流すには多すぎます。集計を一度宣言すれば、それは書き込みパスの中で維持されます。つまり「集計ジョブ」はただ存在しなくなるのです。

```console
kevy-cli -p 6004 HSET reading:1 zone floor1 w 120
kevy-cli -p 6004 HSET reading:2 zone floor1 w 180
kevy-cli -p 6004 HSET reading:3 zone floor2 w 95
kevy-cli -p 6004 IDX.CREATE zone_w ON PREFIX reading: FIELD w TYPE i64 KIND agg GROUPBY zone
kevy-cli -p 6004 IDX.QUERY zone_w GROUP floor1            # [count, sum, min, max, avg]
kevy-cli -p 6004 IDX.QUERY zone_w GROUPS BY sum LIMIT 10  # zones ranked by load
```

アップリンクは、レシピ11のアウトボックスが作業着を着たものです。フィードはすでにコミット済みのすべての書き込みをジャーナルしているので、クラウド同期コンシューマはカーソルループになります。何時間も切れるリンクをまたいで再開可能で、at-least-once・コミット順・クラウドが必要とするものだけにプレフィックスフィルタ済みです。

```console
# needs [feed] enabled = true in kevy.toml (docs/cdc.md)
kevy-cli -p 6004 FEED.TAIL 0
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 100 PREFIX reading:   # the uplink loop
```

レシピ19の`MAXLEN`キャップとTTLを組み合わせてください。生の読み取り値はノード上で有界に保たれ、集計行は小さいまま、フィードカーソルは再起動を生き延びます——kevyそれ自体の他に可動部品ゼロの、エッジの物語の全部です。

## 21. 行の純関数としての派生状態

**SQL 対応：**トリガ層まるごと——`ON DELETE CASCADE`、`UNIQUE` 制約、そしてそれらを信じなくなったあとに書く突合ジョブ——[マトリクス：制約とトリガ](rds-workloads.md#制約とトリガー)。

これは上のレシピ群がずっと周回していたパターンです：§2 のリンクキー、§5 の不変条件、§10 のカスケード、§12 の監査行は、同じ考えを四度当てはめたものです。**一度きちんと述べれば、カスケードも一意性もドリフト検出も同時に片づきます。** これは本番の移行から来ていて、そこへ辿り着くのに一日かかりました——節約する価値のある一日です。

**考え方：**行から、そこに由来するすべてのキーへの**純関数**をひとつ書きます。キーを更新する手続きではなく、「何が存在すべきか」を**返す**関数です。

```rust
// Everything user:42 implies, computed from the row alone.
fn derived(id: &[u8], row: &Row) -> Vec<Vec<u8>> {
    vec![
        key(b"email:", &row.email),          // uniqueness claim
        key(b"dept:", &row.dept, b":users"), // membership
    ]
}
```

すると、あらゆる操作が差分になり、そのどれもが設計されるのではなく**落ちてきます**：

| 操作 | すること | ただで付いてくるもの |
|---|---|---|
| **挿入** | `derived(new)` を足す | 主張とメンバーシップが一緒に現れる |
| **更新** | `derived(new) - derived(old)` を足し、`derived(old) - derived(new)` を消す | 変更された email が**古い主張を解放する**——誰もが手で書いてしまうバグ |
| **削除** | `derived(old)` を消す | カスケードが別のコード経路でなくなる |
| **検証** | 全行で `derived` を再計算し、実在するものと突合 | 設計しなくて済んだドリフト検出器 |

元を取るのは更新の行です。手書きのカスケードはほぼ必ず、新しい主張を足して古いものの解放を忘れます——解放は**チケットで誰も実演しない**ケースだからです。

```rust
store.atomic_all_shards(|ctx| {
    let old = read_row(ctx, id)?;
    let (want, had) = (derived(id, &new), derived(id, &old));

    for k in want.iter().filter(|k| !had.contains(k)) {
        if ctx.exists(&[k]) > 0 { return Err(Taken); }  // uniqueness
        ctx.set(k, id);
    }
    for k in had.iter().filter(|k| !want.contains(k)) {
        ctx.del(&[k]);                                  // release
    }
    write_row(ctx, id, &new)
})
```

`Err` を返せば全体が巻き戻るので（§5）、拒否された書き込みは、行も、半分だけ適用された主張の集合も残しません。

**主張か、インデックスか。** 一意性の主張は**第二の真実の源**であり、行からドリフトしえます。[セカンダリインデックス](indexes.md)は**構造上派生**であり、ドリフトしえません。`atomic_all_shards` の中からは直接引けます：

```rust
if ctx.idx_count(b"email_idx", &want, &want)? > 0 { return Err(Taken); }
```

制限が二つ、どちらも意図的です：

- **`atomic_all_shards` の上だけ。** インデックスの項目は、それが指すキーのシャードに住むので、「この email を持つ行があるか」はすべてのシャードについての問いです。単一シャードの `atomic()` はロックを一つしか持たず、自分の取り分についてしか答えられません——キースペースの 1/N しか見ない一意性検査はほぼ常に「一意」と報告するので、脚注付きで提供するのではなく、**提供しません**。
- **インデックスの読みは、そのトランザクション自身の書き込みを見ません。** 保守はコミット時に走ります。二行を挿入するクロージャは、その二行同士を自分で比べる必要があります。

**検証。** `derived` が関数である以上、**検査器はその関数そのもの**です——同じものを `reconcile` に渡してください：

```rust
let report = store.snapshot().reconcile(
    b"user:",                    // the rows
    &[b"email:", b"dept:"],      // where derived keys live
    |key, row| derived(key, row),
);
if !report.is_clean() {
    warn!("{} missing, {} orphaned", report.missing_count, report.orphaned_count);
}
```

これは**両方向**に突合します。そこが自分で書かずに済ませる価値のある部分です。**欠けたキー**は失われた派生状態ですが、**孤児**——行が消えたのに残っている主張——こそ、半分だけ適用された更新が残すものであり、**あとの挿入を黙って塞ぐ**故障です。欠落だけを探す検査器は、まさにその故障のあいだ「クリーン」と報告します。

これはスナップショット（`store.snapshot()`）に対して、すべてのシャードロックの下で凍結して走るので、並行する書き込みをドリフトと取り違えません。**ただしそれは、書き込み自体が原子的だった場合に限ります**：行とその主張はひとつの `atomic_all_shards` ブロックに入っていなければならず、さもなければ、見つけるべき半適用状態が本当に存在します。突合と原子的書き込みは、同じ保証を両端から見たものです。

起動時に走らせても、定期でも、書き込みを信じきったあとは走らせなくてもかまいません——でも**走らせてください**。「自分が信じている不変条件が、実際に持っている不変条件だ」と言えるものは、これしかないからです。

## 22. PG/MySQL のスキーマを移植する

**SQL 対応：**スキーマファイルそのもの——`CREATE TABLE`、`CREATE INDEX`、`CREATE VIEW`——[マトリクス：セカンダリインデックスの DDL](rds-workloads.md#セカンダリインデックスのddl)。

レシピ 1–8 が手でやることのすべてを、**すでに手元にある** SQL からコンパイルします。`kevy-sql`（そして `kevy-cli sql` という顔）は**宣言時のコンパイラ**です：移行ツールのようにスキーマを**一度だけ**読み、明示的な `TABLE.DECLARE` / `VIEW.CREATE` コマンドと*クエリカード*——`$N` の枠を残した既製の `IDX.QUERY` テンプレート——を出します。サーバの中でクエリごとに走るものは**何もありません**。実行時の場当たり SQL は、エンジン自身が拒み続けます（Law 3）。

対象のスキーマ——[docs/examples/shop.sql](https://github.com/goliajp/kevy/blob/main/docs/examples/shop.sql)、実在の users/orders/order_items を削ったもの：

```sql
CREATE TABLE users (
  id     bigserial PRIMARY KEY,
  email  text,
  name   text,
  plan   text
);
CREATE UNIQUE INDEX ON users (email);

CREATE TABLE orders (
  id          bigserial PRIMARY KEY,
  user_id     bigint,
  status      text,
  total       numeric(10,2),
  created_at  bigint       -- epoch seconds, app-encoded
);
-- INCLUDE = PG covering columns -> kevy stored VALUES (residual FILTER/SORT).
CREATE INDEX ON orders (status) INCLUDE (total, created_at);
-- Multi-column -> a composite ORDERPATH (the (user_id, created_at DESC) walk).
CREATE INDEX ON orders (user_id, created_at DESC);

CREATE TABLE order_items (
  id        bigserial PRIMARY KEY,
  order_id  bigint,
  sku       text,
  qty       int
);
CREATE INDEX ON order_items (order_id);

CREATE VIEW paid_orders AS
  SELECT * FROM orders WHERE status = 'paid';

CREATE VIEW recent_orders_by_user AS
  SELECT id, status, total, created_at FROM orders
  WHERE user_id = $1
  ORDER BY created_at DESC
  LIMIT 20;
```

コンパイルし、宣言をサーバに適用します：

```console
kevy-cli sql compile docs/examples/shop.sql
kevy-cli sql compile docs/examples/shop.sql --apply --url 127.0.0.1:6004
```

コンパイル結果のスクリプト（そのまま）。各テーブルは自分のインデックスを**ひとつの** `TABLE.DECLARE` に畳み込みます。定数のビューはエンジンのビューに、パラメータ付きのビューはクエリカードになります。粗い型の対応づけは、どれも notes で**正直に名指し**されます（kevy の列は `i64|f64|str` だけ——タイムスタンプはアプリ側の符号化で、`serial` は id を割り当ててくれません）：

```text
TABLE.DECLARE users PREFIX users: PK id COLUMN id i64 COLUMN email str COLUMN name str COLUMN plan str INDEX email unique
TABLE.DECLARE orders PREFIX orders: PK id COLUMN id i64 COLUMN user_id i64 COLUMN status str COLUMN total f64 COLUMN created_at i64 INDEX status range VALUES total created_at ORDERPATH user_id_created_at ON user_id THEN created_at DESC
TABLE.DECLARE order_items PREFIX order_items: PK id COLUMN id i64 COLUMN order_id i64 COLUMN sku str COLUMN qty i64 INDEX order_id range
VIEW.CREATE paid_orders QUERY orders.status EQ paid ORDER BY orders.status

# ---- query card: recent_orders_by_user ----
# runtime template — substitute the $N slots and send as-is:
#   $1 = user_id (i64)
#   IDX.QUERY orders.user_id_created_at WHERE user_id EQ $1 LIMIT 20 FIELDS id status total created_at

# notes:
#   - users.id: bigserial → i64, but ids do NOT auto-increment — allocate them app-side (INCR block, cookbook §3)
#   - orders.total: numeric → f64 — fixed-point precision becomes binary float; keep money as integer cents if exactness matters
#   - view paid_orders: read with VIEW.QUERY paid_orders, then hydrate rows with HMGET <key> id user_id status total created_at
```

行はテーブル接頭辞の下の普通のハッシュで（レシピ 1）、コンパイルされた経路はすぐに供せます——`$1` の枠に実際の引数を入れれば、カードはそのまま走ります：

```console
kevy-cli -p 6004 HSET users:1 id 1 email ada@example.com name Ada plan pro
kevy-cli -p 6004 HSET orders:1 id 1 user_id 1 status paid total 19.5 created_at 1700000100
kevy-cli -p 6004 HSET orders:2 id 2 user_id 1 status pending total 5 created_at 1700000200
kevy-cli -p 6004 HSET orders:3 id 3 user_id 2 status paid total 8 created_at 1700000300
kevy-cli -p 6004 HSET order_items:1 id 1 order_id 1 sku sku-7 qty 2
kevy-cli -p 6004 IDX.QUERY users.email EQ ada@example.com
kevy-cli -p 6004 IDX.QUERY orders.user_id_created_at WHERE user_id EQ 1 LIMIT 20 FIELDS id status total created_at
kevy-cli -p 6004 VIEW.QUERY paid_orders LIMIT 10
kevy-cli -p 6004 IDX.QUERY orders.status EQ paid FILTER total RANGE 10 inf
kevy-cli -p 6004 IDX.QUERY order_items.order_id EQ 1 FIELDS sku qty
kevy-cli -p 6004 TABLE.LIST
```

- カードのクエリは `SELECT id, status, total, created_at FROM orders WHERE user_id = 1 ORDER BY created_at DESC LIMIT 20`——複合の走査が供し、新しい順、一跳で列を補います。
- `FILTER total RANGE 10 inf` の行は `INCLUDE` された列に対する**残余述語**です——`WHERE status = 'paid' AND total >= 10` を、**行に触れずに**。
- `order_items.order_id EQ 1` が JOIN を置き換える FK 参照です（レシピ 2）：クエリ二本、クエリ時の join なし。

**拒否が教えます。** コンパイラはクエリ時の評価を要するものをすべて拒みます——**名指しで**、行と列を添えて、それをモデル化するレシピを指して。JOIN なら：

```sql
CREATE VIEW order_emails AS
  SELECT id, email FROM orders
  JOIN users ON users.id = orders.user_id;
```

```text
$ kevy-cli sql compile join.sql
kevy-cli sql: join.sql: line 6, col 3: JOIN is not compilable — kevy
refuses query-time joins (Law 3); model the lookup with an indexed FK
column (IDX.QUERY t.fk EQ …) or app-side assembly (cookbook §2)
```

WHERE が宣言済みのどのアクセス経路にも合わないビューは、**追加すべき宣言を名指しして**エラーになります（`… matches no declared access path — add: CREATE INDEX ON orders (status, total)`）。そして実行時の場当たり SQL には、そもそも扉がありませんでした：

```text
$ kevy-cli -p 6004 SQL SELECT * FROM users
(error) ERR unknown command 'SQL'
```

## レシピ索引

レシピ↔それが置き換えるSQL構文↔意味論と限界を明記する[rds-workloads.md](rds-workloads.md)のマトリクス行、の対応表です。

| # | レシピ | SQL構文 | マトリクス行 |
|---|---|---|---|
| 1 | テーブルと行 | `CREATE TABLE`、ポイント`SELECT` | [テーブル・行・カラム](../rds-workloads.md#tables-rows-columns) |
| 2 | 1対多、多対多 | FKカラム、中間テーブル、`WHERE fk = ?` | [JOIN](../rds-workloads.md#join) |
| 3 | シーケンス | `AUTO_INCREMENT` / `nextval()` | [PK・UNIQUE・AUTO_INCREMENT](../rds-workloads.md#primary-key-unique-auto_increment) |
| 4 | 楽観ロック | バージョンカラムCASの`UPDATE` | [トランザクション](../rds-workloads.md#transactions) |
| 5 | CHECK制約 | `CHECK (…)`＋監査トリガー | [制約とトリガー](../rds-workloads.md#constraints-and-triggers) |
| 6 | 冪等性キー | `UNIQUE INDEX` + `ON CONFLICT DO NOTHING` | [PK・UNIQUE・AUTO_INCREMENT](../rds-workloads.md#primary-key-unique-auto_increment) |
| 7 | ソフトデリート | フラグカラム＋フィルタ付きビュー | [VIEW](../rds-workloads.md#view) |
| 8 | 複合順序付け | `ORDER BY a, b` | [ORDER BY / LIMIT / OFFSET](../rds-workloads.md#order-by--limit--offset) |
| 9 | JSONB | JSONカラム＋生成カラムインデックス | [型システム](../rds-workloads.md#type-system) |
| 10 | カスケード削除／FK | `ON DELETE CASCADE` | [制約とトリガー](../rds-workloads.md#constraints-and-triggers) |
| 11 | 不要になるアウトボックス | トランザクショナル・アウトボックステーブル | [CDC](../rds-workloads.md#cdc) |
| 12 | 監査履歴 | 監査テーブル／binlog考古学 | [CDC](../rds-workloads.md#cdc) |
| 13 | ロールバックウィンドウ | カットオーバー時の逆レプリケーション | [移行プレイブック](migration.md) |
| 14 | 分析エクスポート | ウェアハウスへのETL／binlogタップ | [CDC](../rds-workloads.md#cdc) |
| 15 | ロード順序 | バルク`LOAD DATA`、インデックスは後 | [セカンダリインデックスDDL](../rds-workloads.md#secondary-index-ddl) |
| 16 | TTL付きセッションコンテキスト | セッションテーブル＋期限切れcron | [運用上の差分](../rds-workloads.md#sizing-and-operational-deltas) |
| 17 | エピソード記憶 | 時間`BETWEEN`＋pgvector KNN | [SELECT](../rds-workloads.md#select) |
| 18 | RAGハイブリッド検索 | tsvector＋pgvector、融合 | [SELECT](../rds-workloads.md#select) |
| 19 | センサーキャッシュ | アップサートテーブル＋鮮度cron | [運用上の差分](../rds-workloads.md#sizing-and-operational-deltas) |
| 20 | エッジ集計 | 更新ごとの`GROUP BY`＋ETLアップリンク | [GROUP BYと集計](../rds-workloads.md#group-by-and-aggregates) |
| 21 | 行の関数としての派生状態 | トリガ層まるごと：カスケード、`UNIQUE`、突合 | [制約とトリガー](../rds-workloads.md#constraints-and-triggers) |
| 22 | PG/MySQL スキーマの移植 | `CREATE TABLE` / `CREATE INDEX` / `CREATE VIEW`、コンパイル済み | [セカンダリインデックスの DDL](../rds-workloads.md#secondary-index-ddl) |
