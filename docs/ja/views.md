# ビュー（`VIEW.*` / `view_*`）

ビューとは、**宣言済みインデックスの名前付き合成**です。インデックスの形（shape）からなるAND/OR/DIFFの木に、順序づけ用のインデックスを添えたものを、1つの単位としてクエリできます。評価はクエリごとに行う（**virtual**）か、書き込みのたびに増分維持する（**materialized**、任意でtop-K有界）かを選べます。ビューは「ホットリスト」クエリ——`WHERE state = 'ready' AND pri BETWEEN 0 AND 100 ORDER BY pri DESC LIMIT 10`——に対するエンジンの答えであり、クエリ時のスキャンではなく、宣言されたアクセスパスとして提供されます。

```
IDX.CREATE j_pri  ON PREFIX job: FIELD pri  TYPE i64 KIND range
IDX.CREATE j_state ON PREFIX job: FIELD state TYPE str KIND range

VIEW.CREATE ready_jobs
    QUERY ( AND j_pri RANGE 0 100 j_state EQ ready )
    ORDER BY j_pri DESC
    MODE materialized TOPK 100
VIEW.QUERY ready_jobs LIMIT 10
```

## まず、本当に必要かを確かめる

上の例は意図して選んだもので、そして同時に、**view を必要としない**ことが最も多い形でもあります。`WHERE state = 'ready' AND pri BETWEEN 0 AND 100 ORDER BY pri DESC` は等値の接頭部に、次の列の範囲がひとつ続いたもの——複合 `ORDERPATH` が単一の連続した走査として表すもの、まさにそれです（[tables.md](tables.md#複合-orderpath-の意味論)）：

```console
kevy-cli TABLE.DECLARE job PREFIX job: PK id \
    COLUMN id str COLUMN state str COLUMN pri i64 \
    ORDERPATH ready_by_pri ON state THEN pri DESC
kevy-cli IDX.QUERY job.ready_by_pri WHERE state EQ ready RANGE pri 0 100 LIMIT 10
```

B 木がひとつ、保守すべき second structure はなく、検証すべき再構築もありません。テーブルを宣言した最初の実利用者は、十四本のリスト軸すべてをこのやり方で賄い、view は**ゼロ**個でした——それは欠落ではなく、期待どおりの結果です。

順序付き複合キーでは**なりえない**形のときに、view に手を伸ばしてください：

* 独立したインデックスをまたぐ **`OR`**——順序付きキーの先頭列はひとつなので、和は走査になりません。
* **`DIFF`**——集合の差、左から右を引く。
* 別々の列にかかる**二つの範囲**。複合キーが範囲を取れるのは**末尾**の一列だけで、その後ろには何も続けられません。開いた次元が二つあるものは、ひとつの連続区間ではありません。
* **有界で、常に温かい答え**：`MODE materialized TOPK n` は上位のひと切れを書き込み経路で保守するので、述語がどれだけ絞り込む結果になろうと、読みは固定費です。

あなたの形がこの一覧になければ、`ORDERPATH` を宣言してそこで止めてください。**view は構造上、二つのうち高いほう**です——どのみち存在しなければならないインデックスを組み合わせるものであり、物化すれば書き込み経路に仕事が増え、あとで `VIEW.VERIFY` が監査すべき構造がひとつ生まれます。

## クイックスタート（サーバー）

ビューが合成するものは、すべて先に存在していなければなりません。葉もORDER BY用インデックスも、同じプレフィックスドメイン上で宣言された`IDX.*`インデックスです（[indexes.md](indexes.md)）。

```console
kevy-cli -p 6004 IDX.CREATE j_pri ON PREFIX job: FIELD pri TYPE i64 KIND range
kevy-cli -p 6004 IDX.CREATE j_state ON PREFIX job: FIELD state TYPE str KIND range
kevy-cli -p 6004 VIEW.CREATE ready_jobs QUERY ( AND j_pri RANGE 0 100 j_state EQ ready ) ORDER BY j_pri DESC MODE materialized TOPK 100
kevy-cli -p 6004 HSET job:1 pri 10 state ready
kevy-cli -p 6004 HSET job:2 pri 99 state blocked
kevy-cli -p 6004 VIEW.QUERY ready_jobs LIMIT 10
```

応答は`key, order-value`の組を、ビューの順序で、再開用の`CURSOR`とともにページングします。木の文法は次のとおりです（1行、括弧つき）。

```
tree     = '(' AND|OR|DIFF sub sub ')' | leaf
leaf     = <index> RANGE <min> <max> | <index> EQ <value>
```

補助verb：`VIEW.LIST`（カタログ + モード + 形）、`VIEW.EXPLAIN name`（葉ごとのカーディナリティつきの木）、`VIEW.VERIFY name`（members / bytes / order-exclusions——**bytesとorder-exclusionsはmaterializedのときだけ**です。virtualなビューは何も保存しないので両方0を報告し、しかもそれをVERIFYすると新規の`eval_tree`をまるごと1回支払います。virtualなビューが高くつく唯一の場所がここです）、`VIEW.REBUILD name`（materializedの再構築を強制する——答えは保存されます）、`VIEW.DROP name`。

## 構造上の3つのルール

1. **構成要素は名前付きインデックスである。** 葉は形（`RANGE min max` | `EQ v`。CREATE時に参照先インデックスの型へ強制されます）を持ちます。ビュー層は自前の述語を一切持ちません。木は深さ3以下、葉は4つ以下です。`AND`/`OR`はエンジンが並べ替えることがあり、`DIFF`は左マイナス右で固定です。
2. **ビューが保存するのはメンバーシップと順序だけ**であり、フィールドの値は決して保存しません。`ORDER BY <index>`がソートキーを供給します。順序インデックスに存在しない行は除外されます（数えられ、`VIEW.VERIFY`で可視になります）。
3. **hydrationは参照解決であって、クエリではない。** `VIA <template>`（たとえば`user:{key.1}`。`{key}`はメンバーキー、`{key.N}`はその`:`区切りN番目のセグメント）が各メンバーをターゲットキーへ写像します。`VIEW.QUERY … FIELDS f…`は、2回目の内部ファンアウトで、それらのターゲットを所有するシャード上でフィールドを読みます。ターゲットが存在しなければ行のフィールドはnilになります。ターゲットに述語を付けることはできません。

## モードとコストモデル

- **Virtual** — クエリ時に評価されます。ORDERインデックスを順に流し、候補ごとに木のメンバーシップを判定します。LIMIT 100のページは、O(members)ではなくO(limit / 選択率)回のプローブで済みます。常に最新であり、書き込みコストはゼロです。破綻の形は、大きなORDERドメインの下にある高選択率の木です（1行を出すために多くの候補をプローブすることになります）。その形はmaterializeすべきです。
- **Materialized** — シャードごとの順序つきメンバー集合であり、インデックスを維持するのと同じ書き込みフックの中で更新されます（書き込みごと、参照インデックスごとにプローブ1回。全ビューで共有されます）。`TOPK k`は集合を`k + k/4`に有界化し、ビューの最悪端から追い出します。現在の最悪より悪い非メンバーは、比較1回で棄却されます。[`bench/viewgate.sh`](../../bench/viewgate.sh)が計測した定常状態の書き込み税は、3インデックス + 4つのtop-Kビューでおよそ2%です（ゲートは15%未満にclampしています）。`k`を下回るまで縮むと、シャードローカルな再構築がスケジュールされます（次のtick）。有界でないmaterializedビューは、影響を受ける書き込みごとにO(log members)を支払います。
  - `VIEW.CREATE`の直後には、短い落ち着き期間があると考えてください（新しいtop-K集合に対する最初の書き込みバーストは、追い出しの閾値が安定するまで遅くなります）。

選び方。書き込みの多いドメイン上のダッシュボードtop-100なら`MODE materialized TOPK 100`です（メモリが有界、非候補をO(1)で棄却）。ほどほどのドメイン上でときどきページングされるレポートなら`MODE virtual`です（書き込み税ゼロ、常に最新）。`VIEW.EXPLAIN`は葉ごとのカーディナリティを見せます——この判断のための選択率データです。

### DIFF：宣言されたanti-join

`DIFF`は左マイナス右であり、可換では**ありません**。エンジンが子を決して並べ替えない、唯一の木ノードです。定番の用途は「資格はあるが、除外されていないもの」です。

```
VIEW.CREATE assignable
    QUERY ( DIFF j_pri RANGE 0 100 j_state EQ quarantined )
    ORDER BY j_pri DESC
    MODE virtual
```

——稼働中の優先度帯にあるジョブすべてから、隔離されたものを引いたもの、ということです。

SQLで言えばこれは`WHERE … AND NOT EXISTS (…)`の形にあたります。ただしクエリごとのサブクエリではなく、宣言されたアクセスパスとして提供されます（この語彙の残りは[rds-workloads.md](rds-workloads.md)が写像します）。

## 整合性

インデックスと同じ包絡線です（[indexes.md](indexes.md)）。トリガーとなった書き込みとシャード単位でアトミックであり、シャードをまたぐ場合はグローバルスナップショットなしにマージされます（SCANクラス）。

- 参照しているインデックスのいずれかがまだバックフィル中のあいだ、クエリは`-INDEXBUILDING`を返します（部分的なインデックスは、メンバーシップを黙って誤報告してしまうからです）。リトライの作法はインデックスクエリと同じです。
- `VIEW.REBUILD`は答えを保存します（e2eスイートで表明されています）。`VIEW.VERIFY`はドリフトを反証可能にします（members / bytes / order-exclusions）。
- ビューのカタログはデータディレクトリのサイドカーに永続化されます。materializedの**内容**は派生状態です——再起動後に再構築され、スナップショットには決して入りません。

## 組み込み

`index` cargoフィーチャ（デフォルトで有効）の背後にある型付きAPIです。木は値として渡すので、プロセス内にテキスト文法は登場しません。そしてすべての呼び出しが`KevyResult`を返します（4.0の単一エラー通貨）。

```rust
use kevy_embedded::{
    Config, IndexKind, IndexValType, IndexValue, Store, ViewLeaf,
    ViewMode, ViewTree,
};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(Config::default())?;
    store.idx_create(b"j_pri", b"job:", b"pri", IndexValType::I64,
                     IndexKind::Range)?;
    store.idx_create(b"j_state", b"job:", b"state", IndexValType::Str,
                     IndexKind::Range)?;

    let tree = ViewTree::And(
        Box::new(ViewTree::Leaf(ViewLeaf {
            index: b"j_pri".to_vec(),
            min: IndexValue::I64(0),
            max: IndexValue::I64(100),
        })),
        Box::new(ViewTree::Leaf(ViewLeaf {
            index: b"j_state".to_vec(),
            min: IndexValue::Str(b"ready".to_vec()),
            max: IndexValue::Str(b"ready".to_vec()),   // EQ = same min/max
        })),
    );
    store.view_create(b"ready_jobs", tree, b"j_pri", /*desc*/ true,
                      ViewMode::Materialized { top_k: 100 })?;

    store.hset(b"job:1", &[
        (b"pri".as_slice(), b"10".as_slice()),
        (b"state".as_slice(), b"ready".as_slice()),
    ])?;

    // One page: (Vec<(key, order_value)>, Option<resume_cursor>).
    let (rows, _next) = store.view_query(b"ready_jobs", None, 10)?;
    for (key, val) in &rows {
        println!("{} {val:?}", String::from_utf8_lossy(key));
    }
    Ok(())
}
```

- `view_create(name, tree, order_by, desc, mode)`は同期的にビルドします。参照されるインデックス（葉とORDER BY）はすべて、あらかじめ宣言されていなければなりません（そうでなければ`KevyError::InvalidInput`です）。
- `view_query(name, after, limit)`は`(key, order_value)`の行をページングします。返されるカーソルは排他的に再開します（DESCのビューは大きい側からページングします）。`view_count` / `view_list` / `view_drop`が面を完成させます。
- `VIA`も`FIELDS`もありません——プロセス内の呼び出し側は、自分で参照解決して`hget`でフィールドを読んでください。

## パフォーマンス

計測された包絡線です。clampは[`bench/viewgate.sh`](../../bench/viewgate.sh)にあり、100万行の実サーバーに対して走ります。

- Virtualな`VIEW.QUERY` p99 < 3ms（2要素の木）。
- Materializedな`VIEW.QUERY` p99 < 2ms（インデックス読み出しのライン——materializedのページとは、順序つき集合の読み出しそのものです）。
- 3インデックス + 4つのmaterialized top-Kビューでの書き込み税は、インデックスのみの同一ワークロード比で15%未満（定常状態の実測はおよそ2%）。
- メンバーあたりのメモリは、下の式の±20%以内。

メンバーあたりのメモリは概算で`order_value_width + key_len + 48`バイトで、`VIEW.VERIFY`がライブで報告します。

## 関連項目

- [indexes.md](indexes.md) — ビューが合成するインデックス（宣言、バックフィル、ビューが継承する整合性の包絡線）。
- [rds-workloads.md](rds-workloads.md) — SQLからkevyへの写像の中で、ビューが座る位置。
- [verb-reference.md](../verb-reference.md) — すべての`VIEW.*`形式の、生成された文法。
- [cookbook.md](cookbook.md) — 文脈の中のビューのレシピ。
