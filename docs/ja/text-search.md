# 全文検索（`KIND text`）

textはインデックスエンジンの3番目のkindです。ほかのインデックスと同じように宣言すれば、フィールドの生バイト列がトークン化され、シャードごとの転置セグメントになります。セグメントはすべての書き込みと同期して維持されます（ドリフトゼロ。`range`/`unique`と同じ、構成上の派生物という規律です）。別立ての検索サーバーも、取り込みパイプラインも、アナライザの設定もありません。1つのverbがインデックスを宣言し、以降のすべての書き込みがそれを正確に保ちます。

```
IDX.CREATE posts ON PREFIX post: FIELD body TYPE str KIND text
IDX.QUERY posts MATCH "rust 全文检索" LIMIT 10 [FIELDS title body]
```

## クイックスタート（サーバー）

行は宣言したプレフィックス配下のハッシュキーであり、インデックス対象の値は宣言された1つのフィールドです。`range`インデックスとまったく同じです（[indexes.md](indexes.md)）。

```console
kevy-cli -p 6004 IDX.CREATE posts ON PREFIX post: FIELD body TYPE str KIND text
kevy-cli -p 6004 HSET post:1 title "intro" body "kevy is a pure-Rust key-value store"
kevy-cli -p 6004 HSET post:2 title "search" body "dictionary-free CJK full-text search"
kevy-cli -p 6004 IDX.QUERY posts MATCH "full-text rust" LIMIT 10
```

応答は`key, score`の組のフラットな配列で、良いものが先に来ます。`FIELDS`を足すと、各ヒットを所有するシャード上で、指定したハッシュフィールドを同じ呼び出しの中でhydrateします（2回目の往復はありません）。

```console
kevy-cli -p 6004 IDX.QUERY posts MATCH "search" LIMIT 10 FIELDS title
```

補助verbは、ほかのkindと同じようにtextインデックスにも効きます。

- `IDX.EXPLAIN posts MATCH "full-text rust"` — 実行を伴わないパースです。kind、ビルド状態、推定行数、そしてプランの行を返します。
- `IDX.VERIFY posts` / `IDX.LIST` — entries / bytes / postings / トークン統計を、ライブで。
- `IDX.DROP posts` — 宣言を落とします（カタログの変更であり、サイドカーに永続化されます）。

`MATCH`は`LIMIT`（1000以下）と`FIELDS`を受け取ります。`CURSOR`形式は存在しません（理由は「マッチとランキング」を参照）。

## クイックスタート（組み込み）

プロセス内でも同じエンジンです。textのkindは`text` cargoフィーチャの背後にあります（デフォルトで有効。コンパイルから外すと、`IndexKind::Text`での`idx_create`は`KevyError::Unsupported`を返します）。

```rust
use kevy_embedded::{Config, IndexKind, IndexValType, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(Config::default())?;

    store.idx_create(b"posts", b"post:", b"body", IndexValType::Str,
                     IndexKind::Text)?;   // builds synchronously
    store.hset(b"post:1", &[
        (b"title".as_slice(), b"intro".as_slice()),
        (b"body".as_slice(),
         b"kevy is a pure-Rust key-value store".as_slice()),
    ])?;

    // BM25-ranked hits, best first: Vec<(key, score)>.
    let hits = store.idx_match(b"posts", b"rust store", 10)?;
    for (key, score) in &hits {
        println!("{} {score}", String::from_utf8_lossy(key));
    }
    Ok(())
}
```

`idx_match`は`KevyResult<Vec<(Vec<u8>, f64)>>`を返します。インデックスが見つからなければ`KevyError::NotFound`がそれを名指しします。組み込みに`FIELDS`のhydrationはありません——プロセス内にいるのですから、フィールドは`hget`で読んでください。組み込みのビルドは同期的です。`idx_create`はインデックスが提供可能になった時点で返ります（ポーリングすべき`-INDEXBUILDING`の局面はありません）。

## トークン化（辞書不要）

- ラテン文字・英数字の連なり → 小文字化された語トークン（長さ2以上）。
- CJK（統合漢字、かな、ハングル）→ **隣接するbigram**。「全文检索」は全文/文检/检索としてインデックスされるので、任意の2文字部分列のクエリが辞書なしでマッチします。孤立したCJK文字は、それ自身を出力します。
- トークンがスクリプト境界をまたぐことはありません。クエリも同じ規則でトークン化されます。

bigram方式がCJK対応のすべてです。出荷すべき辞書もなければ、古くなる辞書もありません。混在スクリプトの文書（`"Rust 入门"`）は、両方の半分をそれぞれの規則でインデックスします。代償は、1文字のCJKクエリにおける再現率のノイズです（それらは孤立文字として出力されたものにしかマッチしません）。2文字以上でクエリしてください。

## マッチとランキング

`MATCH`は**クエリトークン上のORセマンティクスで、BM25でランクづけ**します（k1=1.2、b=0.75、非負idfのバリアント）。より多くの、そしてより稀なクエリ語にマッチする文書ほど上位に来ます。語の頻度は飽和し、長い文書は正規化により下げられます。

**スコアはシャードごとではなく、グローバルです。** `df`、`n_docs`、`avgdl`はコーパス全体のもので、クエリ時に集めます。パス1が各シャードにクエリ自身の語の件数を尋ね、原点がそれらを合算し、パス2がその合計に対して全シャードをスコアリングします。つまりクエリは、1シャードでも8シャードでも同じランキングと同じスコアを返します——それをそのまま断言するテストがあり、CIで走っています。

定期スナップショットではなく、クエリ時です。キャッシュしないので陳腐化の窓を文書化する必要がなく、書き込みパスでは何も調整しません。2回目のラウンドは、100万件の文書に対する28msのp95のうち0.06msです（実測——[PERF-LEDGER.md](../../bench/PERF-LEDGER.md)）。パス1が実際にクエリされた語の`(term, df)`ペアしか動かさないからです。

宣言された近似は、あと1つ残っています。

- **カーソルはありません。** BM25の深いページングはアンチパターンです（ページNを出すには、その上にあるものすべてを再スコアリングする必要があります）。`LIMIT`は1000で頭打ちです。

その表層のクローズは、いまやすべて実行されます：

| クローズ | 何をするか |
|---|---|
| `IN <field…>` | それらのフィールド内だけでスコアリングします——フィールド限定のBM25（自前の頻度、自前の長さ）であって、文書全体のスコアへのフィルタではありません |
| `FILTER <field> RANGE\|EQ …` | 保存済みの値に対する非スコアリングの述語です。top-Kの前に適用されるので、40位の適格文書でも`LIMIT 10`のページに届きます |
| `SORT <field> ASC\|DESC` | スコアではなく保存済みの値で選びます。値を持たない文書は、どちらの向きでも最後に並びます |
| `DISTINCT <field>` | 1つの値につき最大1ヒット、そのグループの最良のものだけ。選択の途中で畳まれるので、ページは`LIMIT`行を保ちます |
| `FACET <field…>` | ページではなく**マッチ集合全体**に対する値の件数です。`FILTER`は絞り込みますが、`DISTINCT`は絞り込みません |
| `HIGHLIGHT [field…]` | マッチした語のバイト範囲を、フィールドごとに返します |
| `TYPO 0\|1\|2` | 裸の語に対する編集距離の許容です（フレーズと接頭辞は厳密なまま） |
| `LIMIT n` / `OFFSET m` / `FIELDS f…` | ページサイズ、ページオフセット、ハイドレーション |

`"引用符つきフレーズ"`と`word*`の接頭辞は、`MATCH`のテキスト自体に書きます。`FILTER`、`SORT`、`DISTINCT`、`FACET`は、インデックスが`VALUES`宣言時に保存したフィールドを読みます。保存していないフィールドを指定するとエラーになり、保存しているフィールド名が返ります。

受理はするが無視するクローズは、エラーより悪いでしょう——落とされた`FILTER`はフィルタされていない行を返します。それは成功応答をまとった誤答です。だからキーワードは、どれ一つ動く前に凍結され、名前で拒否されていました。

（これは以前の境界を反転させたものです。このページの旧版は、フレーズクエリとブールクエリを意図的にスコープ外だと述べていました——「それらが必要なら、あなたは検索エンジンを説明している」と。ランクづけされた参照で止まるtext kindにとってそれは正しく、そして目標が変わりました。）

## ハイブリッド検索（BM25 + KNN）

同じコーパス上のtextインデックスとANNインデックスは、reciprocal-rank fusionによってサーバーサイドで融合します。

```
IDX.QUERY HYBRID posts MATCH "rust storage" embs KNN <f32-le-blob>
    [LIMIT n] [RRFK k] [EF ef] [FIELDS f…]
```

各ヒットの融合スコアは、2つの結果リストにわたる`Σ 1/(k + rank_i)`です（`RRFK`のデフォルトは60——標準的な融合定数です。生のスコアではなく順位を使うので、BM25と距離を互いに較正する必要がありません）。KNN側は[vector-search.md](vector-search.md)を参照してください。HYBRIDはサーバーのverbです。組み込みの呼び出し側は`idx_match`と`idx_knn`を走らせ、プロセス内で融合してください。

## ライフサイクルと整合性

どのインデックスkindとも同じ包絡線です（[indexes.md](indexes.md)）。

- 書き込みとそのセグメント更新は、**所有シャードの内側でアトミック**です（単一のリアクタースレッド / シャードロック）。更新は、古い文書のトークンをちょうど過不足なく取り除き、新しいものを挿入します。そのために文書は元のテキストを保持しており、tombstoneのドリフトは起きません。
- 宣言されたフィールドが欠けている行は、単にトークンを寄与しません。`TYPE str`のtextフィールドに、型強制の失敗という概念はありません。
- シャードをまたぐクエリは、グローバルスナップショットなしに、シャードごとのtop-Kをマージします（SCANクラス。`DBSIZE`と同じです）。
- **サーバーのバックフィルは非同期です。** 稼働中のキー空間に対して`IDX.CREATE`した直後、あるいは再起動の直後は、再構築が完了するまでクエリは`-INDEXBUILDING`を返します（データの可用性がインデックスのビルドを待つことは決してありません。ポーリングするかリトライしてください——[indexes.md](indexes.md)を参照）。組み込みのビルドは同期的です。
- 転置セグメントは**派生状態**です——スナップショットにもAOFにも決して記録されず、再起動後にデータから再構築されます。
- `IDX.CREATE`時の`MAXMEM`はセグメントのメモリに上限を設けます。予算を超えるビルドは、際限なく成長するのではなく、宣言的に失敗します（クエリに`-INDEXOVERBUDGET`が返ります）。

## パフォーマンス

top-Kの評価にはMaxScoreによる枝刈りを使います（稀な語から先に処理し、より一般的なリストは、新しい文書をtop Kへ押し上げられなくなった時点から候補ごとのプローブに切り替わります）。postingsはdoc-idの転置リストで、2通りにインパクト順序づけされています。tfのバケットは降順、各バケットの内側では疎なlog2 dlバンドが昇順です。単一語のクエリ（枝刈りの相手となる2本目のリストがない場合）は、下端がk番目の床を超えられない最初のバンドでちょうど停止します。おかげで最も一般的な語ですら、20万文書でおよそ0.1msで答えます（postingsの全走査だった頃はおよそ6msでした）。postingsが1件だけのトークン（Zipfのロングテール）はインラインにとどまり、ヒープを消費しません。

計測された包絡線です（領収書はbenchツリーにあります）。

- [`bench/textgate.sh`](../../bench/textgate.sh)は、実サーバーに対して、100万件の混在スクリプト文書（各およそ100バイト）で`MATCH` p95 < 20msをゲートし、あわせてメモリの式を実RSSの増分に対してゲートします。CIに隣接するリリースチェックの中で走ります——これらの数値は願望ではなく、clampです。
- [`bench/PERF-LEDGER.md`](../../bench/PERF-LEDGER.md)が比較対決を記録しています。同一コーパス上のRediSearchの`FT.SEARCH`に対して、BM25 top-10でqps +21%、p95は互角です。

書き込み側は標準的なインデックス税です。マッチするインデックス1つにつき、書き込みごとにハッシュフィールド読み出し1回とセグメント更新1回。空のカタログのコストは、書き込みごとに1回の分岐しない分岐です。

## サイジング

`bytes ≈ Σ_token (token_len + 48) + postings × 64 + Σ_doc (key_len + text_len + 72)`（文書は元のテキストを保持するので、更新は自分のトークンだけをちょうど取り除けます）。`IDX.VERIFY` / `IDX.LIST`がライブで報告します（textのkindではentries/bytes/postings/tokens）。`bench/textgate.sh`がこの式を実RSSの増分に対してゲートします。

再構築は標準のバックフィルの骨組みに乗ります（サーバーではtick増分、組み込みでは同期）。

## 関連項目

- [indexes.md](indexes.md) — このkindが差し込まれるインデックスエンジン（宣言の文法、カーソルの契約、整合性の包絡線）。
- [vector-search.md](vector-search.md) — ANNのkindと、`HYBRID`のもう半分。
- [verb-reference.md](../verb-reference.md) — すべての`IDX.*`形式の、生成された文法。
- [cookbook.md](cookbook.md) — 文脈の中の全文検索レシピ。
