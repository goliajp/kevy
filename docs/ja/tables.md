# テーブル（`TABLE.*` / `table_*`）

テーブルは、**名前のついた、検証可能な宣言**です。宣言の時点で、kevy がすでに持っているインデックスとビューのプリミティブへコンパイルされます。`TABLE.DECLARE` はプレフィックス、型付きカラム、セカンダリインデックス、複合ソートパスを受け取り、普通の名前つきインデックスを生成します。クエリ時に動く新しいものは何もありません。これはエンジンの常設ルール（Law 3）のユーザー向けの言い直しです。**kevy はクエリを決してプランしません——アクセスパスに名前をつけるのはあなたです**。テーブルは、アクセスパスの一族にまとめて名前をつけるための、人間工学的な書き方です。

```
TABLE.DECLARE user PREFIX u: PK id
    COLUMN id str COLUMN name str COLUMN age i64
    COLUMN dept str COLUMN email str
    INDEX age range VALUES dept name
    INDEX email unique
    ORDERPATH by_dept_age ON dept THEN age DESC

IDX.QUERY user.by_dept_age WHERE dept EQ eng LIMIT 20
```

行はこれまでどおり、プレフィックス配下のハッシュキーです——テーブルを宣言しても書き方は何ひとつ変わらず（`HSET u:1 name alice age 30 …`）、**スキーマも課されません**。宣言済みカラムを欠いた行は、そのカラムが NULL の行です（すべてのインデックスがもともと持つ、フィールド欠落の意味論）。宣言が買ってくれるのは、コンパイル済みのアクセスパスと、`VERIFY` の面と、その全体をひとつの動詞で扱うライフサイクルです。

## 宣言モデル

`TABLE.DECLARE` は各句を名前つきインデックスへコンパイルします。

| 句 | コンパイル先 |
|---|---|
| `INDEX <col> range\|unique [VALUES <col>…]` | プレフィックス上の `<table>.<col>` という名のスカラーインデックス。`VALUES` のカラムは行ごとに保存されます（型はカラム宣言から） |
| `ORDERPATH <name> ON <col> [DESC] [THEN <col> [DESC]]…` | `<table>.<orderpath>` という名の複合 range インデックス——行ごとに順序を保つバイト列キーが 1 本 |

コンパイルされた名前はひとつの名前空間を共有します——`<table>.<col>` と `<table>.<orderpath>`——ので、インデックス済みカラムと同名の ORDERPATH は宣言時に、名前つきで拒否されます。コンパイルはサーバーと組み込みストアが共有する単一の実装で（dispatch oracle が CI で両面をバイト比較します）、しかも**原子的**です。どんなエラーでも何もインストールされません——半分だけ宣言されたテーブルは存在しません。

コンパイルされたインデックスがすることは、手書きの `IDX.CREATE` がすることと同じです。同じ埋め戻しの挙動、同じ `-INDEXBUILDING` の規律、同じサイドカー永続化、同じ予算による拒否（[indexes.md](indexes.md)）。`TABLE.DROP` はテーブルと、それがコンパイルしたすべてのインデックスを落とします。

## 文法

```
TABLE.DECLARE name PREFIX p PK col
    COLUMN name i64|f64|str [COLUMN ...]
    [INDEX col range|unique [VALUES col ...]] ...
    [ORDERPATH name ON col [DESC] [THEN col [DESC]] ...] ...
TABLE.DROP name        # drops the table + its compiled indexes; 1|0
TABLE.LIST             # name/prefix/pk + column/index/orderpath counts
TABLE.VERIFY name      # component fsck + a bounded column spot check
```

- カラム型は `i64 | f64 | str`——スカラーインデックスの型そのものです。それ以外（タイムスタンプ、ブール、列挙）はアプリ側でこの 3 つのどれかにエンコードし、粗い対応づけは隠さずに明言されます（kevy-sql は型変換されたカラムごとに注記を出力します）。
- `PK` は宣言済みカラムを指します。これはドキュメントであり、`VERIFY` の面です——行はこれまでどおりキーで指されます。`serial` 式の id 割り当てはレシピ（[シーケンスのレシピ](cookbook.md#3-sequences)）であって、エンジンの機能ではありません。
- テーブルは最大 64 個。構造上の拒否はすべて名前つきです（重複カラム、未知の `VALUES` カラム、名前衝突、……）。黙って通ることはありません。

`TABLE.VERIFY` は、コンパイルされた各インデックスのドリフト再チェック（`IDX.VERIFY` のカウンタ——entries / bytes / coerce_failures / duplicates / drift / checked、インデックスごと）に加え、有界の抜き取り検査を走らせます——shard ごとに最大 64 行をサンプルし、*存在する*宣言済みカラムのすべてが宣言どおりの型に変換できることを表明します（欠落は NULL であって、エラーではありません）。構成インデックスのどれかがまだ埋め戻し中なら、`-INDEXBUILDING` を答えます。

## 複合 ORDERPATH の意味論

ORDERPATH は[複合順序のレシピ](cookbook.md#8-composite-ordering-order-by-a-b)——`ORDER BY a, b DESC` の歩き方——を、本物の複合インデックスに機械化します。行ごとに順序を保つバイト列が 1 本あり、リレーショナルの複合インデックスと同じやり方で、1 本の B-tree がクエリに答えます。ルールは次のとおりです。

- **`WHERE` は先頭プレフィックスを取ります。**`WHERE a EQ x [b EQ y …] [RANGE c min max]` は、複合インデックスのカラムを宣言順に先頭から指名しなければなりません。等値のプレフィックス、次に*その次の*カラムへの range を最大 1 つ。それ以降は無制約です（古典的な複合 B-tree の意味論）。プレフィックスでないカラムの指名は名前つきエラーです——スキャンには決してなりません。
- `RANGE` は `WHERE` の中で終端です——その後には何も続けられません。range の後ろの条件は、1 回の連続した歩きでは表現できないからです。
- **成分ごとの `DESC`** は保存順に反映されるので、`ON dept THEN age DESC` は各部門の行を、再ソートなしで最も古く大きい端からページングします。
- **成分カラムをひとつでも欠く行は、複合インデックスから除外されます**（型変換の失敗も同様）——他のすべてのアクセスパスからは完全に見えたままです。**255 バイト**を超える `str` 成分も行を除外します。リレーショナルの B-tree がインデックス行サイズに課すのと同種の上限で、range の境界を厳密に保つものです。成分は最大 8 つです。
- `WHERE` は `IDX.COUNT` にも効き、複合カラムを宣言していないインデックスの上では名前つきで拒否されます。

```
IDX.QUERY user.by_dept_age WHERE dept EQ eng                  # all eng, age DESC
IDX.QUERY user.by_dept_age WHERE dept EQ eng RANGE age 31 46  # eng, 31<=age<=46
```

## テーブルをクエリする

クエリはコンパイルされた名前への `IDX.QUERY` のままです——テーブルはクエリの動詞を増やしません。エンジンはクエリ時に何も評価しないからです。

```
IDX.QUERY user.age RANGE 25 45                          # driving range
IDX.QUERY user.email EQ d@x                             # unique point lookup
IDX.QUERY user.age RANGE 0 100
    FILTER dept EQ eng SORT name ASC LIMIT 20 OFFSET 20 # clauses on VALUES
IDX.QUERY user.age RANGE 0 100 FACET dept
IDX.QUERY user.by_dept_age WHERE dept EQ eng LIMIT 20 FIELDS name email
```

`FILTER` / `SORT` / `DISTINCT` / `FACET` / `OFFSET` が読むのは、インデックスが **`VALUES` 宣言の時点で保存した**カラムです——全文検索の原型と同じ句の文法、同じ「shard をまたいで厳密」の意味論です（[text-search.md](text-search.md)）。`FILTER` はページより前に適用されるので、深い順位の適格行も `LIMIT` に届きます。`FACET` はマッチ集合全体を数え、欠けた値はどちらの向きでも最後に並びます。インデックスが保存していないフィールドの指名はエラーで、保存しているフィールドの名前を挙げて答えます。駆動する述語は常に、インデックスされた range / EQ / WHERE です——インデックスなしの `WHERE` は存在しません。

**index-only クエリは行にゼロ回しか触れません。**FILTER / SORT / COUNT のクエリは RAM 常駐のインデックスだけで答えます——行読み取りカウンタはゲートスイートで `== 0` と表明されています（`bench/tablegate.sh`）。これが、この 2 つの機能を一緒に設計した狙いのティアリング相乗効果です。[透過的ティアリング](tiering.md)をオンにすれば、全行コールドなテーブルが index-only クエリを**ディスク読み取りゼロ**で捌き、最後の hydration ページ（`FIELDS …`）だけがコールド読み取りを払います——1 行 1 回、バッチで。`VALUES` カラムのないインデックスは、メモリとクエリパスにおいて、それを一度も宣言しないストアのインデックスとバイト単位で同一です（宣言しなければゼロコストのゲート）。

## NULL、一意性、そして何が強制されるか

- **NULL = 欠けたフィールド。**必須のカラムはありません。インデックス対象カラムを欠く行は、単にそのインデックスにいないだけです。エンジンの `CHECK` も、既定値も、NOT NULL もありません——制約はレシピです（[制約のレシピ](cookbook.md#5-check-constraints-and-multi-key-invariants)、アトミックブロック）。
- テーブル層の**一意性は、強制ではなく検証です**。`unique` インデックスは `IDX.CREATE KIND unique` が築くのと同じフェンスで（[indexes.md](indexes.md#uniqueness-is-a-fence-not-a-lock)——予約パターンで競合なしにできます）、`TABLE.VERIFY` は `duplicates` を報告します。エンジンが後から書き込みを拒否するのではありません。

## それが「ではない」もの

拒否として述べます。エンジンは近似する代わりに、名前つきで拒否するからです。**ランタイム SQL はありません**（サーバーへ送るのは `TABLE.DECLARE` であって `CREATE TABLE` ではありません）。**クエリ時の join はありません**（ビューの `VIA` 参照解決は別です——[views.md](views.md)）。**HAVING / サブクエリ / 式はありません**。**エンジンによる制約の強制はありません**。それぞれの SQL から kevy への対応は [rds-workloads.md](rds-workloads.md) に、動くレシピは [cookbook.md](cookbook.md) に、スキーマのコンパイル経路はすぐ下にあります。

## kevy-sql——スキーマは送るのではなく、コンパイルする

`kevy-sql`（とその `kevy-cli sql` の顔）は**宣言時コンパイラ**です——マイグレーションツールのように、PG/MySQL 方言のスキーマファイルを一度だけ読み、明示的な宣言を出力します。

```console
kevy-cli sql compile schema.sql                          # print the plan
kevy-cli sql compile schema.sql --apply --url 127.0.0.1:6004
```

- `CREATE TABLE` → `TABLE.DECLARE`（型は `i64|f64|str` へ粗く対応づけ、対応づけごとに正直に注記されます）。
- `CREATE [UNIQUE] INDEX` → `INDEX` 句。PG の `INCLUDE` カバリングカラム → 保存された `VALUES`。複数カラムのインデックス → `ORDERPATH`。
- 定数の単一テーブル `CREATE VIEW … AS SELECT` → エンジンのビュー。パラメータつきなら → **クエリカード**——`$N` のスロットをアプリが埋める、出来合いの `IDX.QUERY` テンプレートです。
- コンパイラもプランしません。ビューを、あなたが宣言したアクセスパスに突き合わせ、合うものがなければ、どの宣言を足すべきか（`add: CREATE INDEX ON t (dept, age)`）を告げます。スキャンを発明することはありません。アドホック SQL、join、サブクエリ、`OR`、`GROUP BY` の類は `line:col` つきで拒否され、置き換えるレシピを指し示します。

端到端のウォークスルー——実物の users/orders/order_items スキーマをコンパイルし、適用し、クエリするまで——は[スキーマ移植のレシピ](cookbook.md#22-porting-a-pgmysql-schema)です。

## 組み込み

型付き API、同じコンパイル。プロセス内ではテキスト文法は不要です（宣言型——`TableSpec`、`TableIndex`、`OrderPath`——は、唯一のコンパイラを持つ crate である `kevy-index` にあります）。

```rust
use kevy_index::TableSpec;

store.table_declare(spec)?;          // TableSpec, validated + compiled,
                                     // indexes built synchronously
let tables = store.table_list();
let report = store.table_verify(b"user")?;   // per-index counters + spot check
store.table_drop(b"user");
```

ワイヤ形式（`db.cmd("TABLE.DECLARE", …)`）も使え、同一の共有文法でパースされます——サーバーと組み込みのバイト一致は、CI の dispatch oracle が固定しています。

## 性能

ゲートの締めつけと、その測定状態を率直に述べます。適合 / 一致 / 拒否 / index-only の表明は、このツリーで緑で走っています（`bench/tablegate.sh`）。**スループットの締めつけ**——10 M 行でのインデックス点参照 p99 ≤ 1 ms、10 M 行での FILTER+SORT+LIMIT-20 ページ p95 ≤ 5 ms、インデックス 3 本 + 宣言済み VALUES の書き込み税 ≤ 15 %（素の `HSET` 比）——は perfgate のメトリクス行で、そのベースラインは**専用ベンチマシン待ち**です（`bench/capacity-envelope.sh` が記録します）。記録されるまで、これらはターゲットであって測定値ではありません——このページは、それらを結果として引用しません。

書き込みコストは標準的なインデックス税です。コンパイルされたインデックス 1 本につき、合致する書き込みごとにフィールド読み取り 1 回とセグメント更新 1 回。空のカタログのコストは、取られない分岐 1 つです。

## 参照

- [indexes.md](indexes.md)——テーブルのコンパイル先であるインデックスエンジン。
- [tiering.md](tiering.md)——一緒に設計されたもう半分。インデックスはホット、行はコールド。
- [rds-workloads.md](rds-workloads.md)——SQL 語彙の完全な対応表(何がコンパイルでき、何がレシピで、何が拒否されるか)。
- [cookbook.md](cookbook.md)——複合順序とスキーマ移植のレシピ。
- [views.md](views.md)——同じインデックスの上の、名前つきの合成。
