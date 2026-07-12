# セカンダリインデックス（`IDX.*` / `idx_*`）

kevyは、キー空間の**プレフィックスドメイン**に対して宣言型のセカンダリインデックスを維持できます。プレフィックス配下のハッシュキーが1つの「行」であり、宣言された1つのハッシュフィールドがインデックス対象の値です。インデックスは**すべての書き込みと同期して**維持され（構成上の派生物——インデックスがデータからドリフトすることは原理的にあり得ず、`IDX.VERIFY`がそれを反証可能にします）、カーソルページング、2インデックスの合成、任意のフィールドhydrationとともにクエリできます。

```
IDX.CREATE idx_age ON PREFIX user: FIELD age TYPE i64 KIND range
HSET user:42 age 31 name "……"
IDX.QUERY idx_age RANGE 18 30 LIMIT 100 FIELDS name
```

## 宣言する

`IDX.CREATE <name> ON PREFIX <p> FIELD <f> TYPE i64|f64|str KIND range|unique [MAXMEM <bytes>]`

- **TYPE**はスカラーへの型強制です。フィールドが欠けている行、あるいはパースに失敗する行は**除外されます**（インデックスごとに数えられ、`IDX.VERIFY` / `IDX.LIST`が`coerce_failures`として報告します。これは宣言的なフェンスであって、実行時エラーではありません）。
- **KIND range**は`RANGE min max`のスキャンを提供します。**unique**は同じものに加えて、重複フェンス（後述）を提供します。
- **MAXMEM**はインデックスのメモリに上限を設けます。予算を超えるビルドは、際限なく成長するのではなく、宣言的に失敗します（クエリに`-INDEXOVERBUDGET`が返ります）。
- インデックスは最大64個です。カタログはデータディレクトリのサイドカーに永続化されます。インデックスの**内容**は派生状態です——スナップショットにもAOFにも決して記録されず、再起動後にバックグラウンドで再構築されます（準備できるまで`-INDEXBUILDING`。データの可用性が待たされることはありません）。

## クエリする

- `IDX.QUERY <name> RANGE <min> <max> | EQ <v> [LIMIT n] [CURSOR c] [FIELDS f…]` → `[next-cursor, rows]`。行は`(value, key)`で、全シャードにわたって順序づけられます。`FIELDS`は、各行を所有するシャード上で指定されたハッシュフィールドをhydrateし（2回目の往復は発生しません）、行をネストした`[key, value, fname, fval…]`の形に切り替えます。
- `IDX.QUERY COMPOSE AND|OR <n1> <spec1> <n2> <spec2> …` — 2インデックスの合成です。**キー順**になります（2つの値ドメインが異なるため）。LIMIT/CURSOR/FIELDSの末尾は同じです。AND/ORはシャードごとに走ります（キーはちょうど1つのシャードに住むので、シャードごとの集合代数がグローバルに合成されます）。
- `IDX.COUNT <name> RANGE|EQ …` — キーを実体化せずに数えます。
- `IDX.VERIFY <name>` — 合算した統計。entries、bytes、coerce_failures、duplicates。
- `IDX.LIST` — カタログと、インデックスごとのstate/entries/bytes。
- カーソルの契約はSCANクラスです。走査の全体を通じて安定していた行はちょうど1回見えます。並行する挿入・削除は現れるかもしれないし、現れないかもしれません。`"0"`は開始または枯渇を意味します。

## 一意性はフェンスであって、ロックではない

`unique`インデックスは**書き込みをブロックしません**。書き込み時にグローバルな一意性を強制すれば、シャードをまたぐ書き込みを直列化することになるからです。代わりに、重複は数えられ（VERIFY/LISTの`duplicates`）、`EQ`読み出しが複数ヒットする形で可視になります。硬い一意性が必要なら、クラスタモードで`{hashtag}`プレフィックスによりドメインを1シャードに固定するか、`MULTI`/`WATCH`のもとでcheck-then-writeしてください。

## 組み込み

同じエンジンを型付きAPIで使います。`idx_create / idx_drop / idx_query / idx_count / idx_stats / idx_list`（値は`IndexValue`、カーソルは`IndexCursor`）。`FIELDS`のhydrationはありません——プロセス内にいるのですから、フィールドは`hget`で読んでください。`idx_create`は同期的にビルドし、インデックスが提供可能になった時点で返ります。

## 整合性とコストモデル

- 書き込みとそのインデックス更新は、所有シャードの内側でアトミックです（単一のリアクタースレッド / シャードロック）。シャードをまたぐクエリは、グローバルスナップショットなしにシャードごとにマージされます（SCANクラス。`DBSIZE`と同じです）。
- **空のカタログのコストは、書き込みごとに1回の分岐しない分岐**です（Relaxedなアトミックロード1回）。インデックスが宣言されている場合、インデックス対象ドメインへの書き込みは、マッチするインデックス1つにつき、ハッシュフィールド読み出し1回とB木更新1回を支払います。
- インデックス1つあたりのメモリは概算で`rows × (value_width + avg_key_len + 48)`バイトです（定数はエントリごとの構造オーバーヘッド）。`IDX.LIST`が実測バイト数を報告し、`bench/idxgate.sh`がこの式をゲートします。

## 集約kind（`KIND agg`）——書き込み時GROUP BY

```
IDX.CREATE ord_amt ON PREFIX ord: FIELD amount TYPE i64 KIND agg GROUPBY status
IDX.QUERY ord_amt GROUP paid                      → [count, sum, min, max, avg]
IDX.QUERY ord_amt GROUPS BY sum LIMIT 100         → ranked [group, count, sum, min, max]
```

`SELECT g, COUNT(*), SUM(v) … GROUP BY g`に対するエンジンの答えです。集約は**書き込みパスの中で**維持されます（宣言されたアクセスパスであって、クエリ時の行スキャンでは決してありません）。min/maxはグループごとの値の多重集合により、削除のもとでも正確なままです。sumはf64で累積されます（精度の限界は文書化済み）。値の型強制に失敗した行、あるいはグループフィールドが欠けている行は除外され、数えられます（VERIFY）。シャードをまたぐマージは正確です。countとsumは足し合わされ、極値は取り合わされます。`GROUPS`はcount/sum/maxの降順、またはminの昇順でランクづけし、LIMITは1000以下です。

HAVINGなし、集約式なし、近似スケッチなし——それはクエリ言語への坂道です。`GROUPS`の結果はアプリ側でフィルタしてください。

組み込みでは`idx_create_agg(name, prefix, field, ty, group_by)` / `idx_group(name, g)` / `idx_groups(name, by, limit)`です。

メモリは概算で`groups × (gkey+64) + distinct_values × 18 + rows × (key+10)`です（定数は実測RSSに対して較正済み）。GROUP p99 < 1ms @ 1M×10kグループ、GROUPSのtop-100 < 5ms、書き込み税 < 10%とあわせて、`bench/agggate.sh`が実RSSに対してゲートします。
