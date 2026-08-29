# ベクトル検索（`KIND ann`）

ANNはインデックスエンジンの4番目のkindです。ほかのインデックスと同じように宣言すれば、フィールドの生バイト列がf32ベクトルとしてパースされ、シャードごとのHNSWグラフにインデックスされます。グラフはすべての書き込みと同期して維持されます。サイドカーのベクトルデータベースも、取り込みジョブもありません。埋め込みはレコードの残りと同じハッシュ行の中に住み、グラフがデータからドリフトすることは原理的にあり得ません。

```
IDX.CREATE embs ON PREFIX doc: FIELD v TYPE vector KIND ann DIM 768
    [DISTANCE cosine|l2|ip] [M 16] [EF 200]
IDX.QUERY embs KNN <f32-le-blob> LIMIT 10 [EF 400] [FIELDS title]
IDX.REBUILD embs
```

## クイックスタート（サーバー）

行は宣言したプレフィックス配下のハッシュキーであり、インデックス対象の値は、生のベクトルバイト列を保持する宣言された1つのフィールドです。

```console
kevy-cli -p 6004 IDX.CREATE embs ON PREFIX doc: FIELD v TYPE vector KIND ann DIM 4
kevy-cli -p 6004 HSET doc:1 title "intro" v "csv:0.1,0.2,0.3,0.4"
kevy-cli -p 6004 HSET doc:2 title "search" v "csv:0.4,0.3,0.2,0.1"
kevy-cli -p 6004 IDX.QUERY embs KNN "csv:0.1,0.2,0.3,0.4" LIMIT 2 FIELDS title
```

応答は`key, distance`の組の昇順です（最も近いものが先）。`FIELDS`は、各ヒットを所有するシャード上で、同じ呼び出しの中でハッシュフィールドをhydrateします。本番では、ベクトルの値はクライアントライブラリが書くバイナリのblob（リトルエンディアンf32の`dim × 4`バイト）です。上の`csv:`形式はデバッグ用の便宜です。

補助verbは、ほかのどのkindとも同じように効きます。`IDX.EXPLAIN embs KNN …`（実行を伴わないパースとプラン）、`IDX.VERIFY` / `IDX.LIST`（ライブの統計）、`IDX.DROP`。`IDX.REBUILD`はANN固有です——「削除と再構築」を参照してください。

## クイックスタート（組み込み）

ANNのkindは`vector` cargoフィーチャの背後にあります（デフォルトで有効。コンパイルから外すと`KevyError::Unsupported`を返します）。

```rust
use kevy_embedded::{AnnSpec, Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(Config::default())?;

    // m / ef of 0 select the defaults (16 / 200);
    // distance: 0 = cosine, 1 = l2, 2 = ip.
    store.idx_create_ann(b"embs", b"doc:", b"v", AnnSpec {
        dim: 4, distance: 0, m: 0, ef: 0,
    })?;

    let v1: Vec<u8> = [0.1f32, 0.2, 0.3, 0.4]
        .iter().flat_map(|f| f.to_le_bytes()).collect();
    store.hset(b"doc:1", &[
        (b"title".as_slice(), b"intro".as_slice()),
        (b"v".as_slice(), v1.as_slice()),
    ])?;

    // Nearest neighbors ascending: Vec<(key, distance)>.
    // ef = 0 takes the engine default beam width.
    let hits = store.idx_knn(b"embs", &[0.1, 0.2, 0.3, 0.4], 10, 0)?;
    for (key, dist) in &hits {
        println!("{} {dist}", String::from_utf8_lossy(key));
    }
    Ok(())
}
```

`idx_knn`は`KevyResult<Vec<(Vec<u8>, f32)>>`を返します（`k`は1..=1000にclampされます）。インデックスが見つからなければ`KevyError::NotFound`が、`AnnSpec`が不正（次元が0、未知の距離タグ）なら`KevyError::InvalidInput`がそれを名指しします。組み込みのビルドは同期的です——`idx_create_ann`はインデックスが提供可能になった時点で返ります。プロセス内に`FIELDS`のhydrationはありません。フィールドは`hget`で読んでください。

## ワイヤ形式

ベクトルフィールドは、リトルエンディアンf32の`dim × 4`バイトを保持します（RediSearchの慣習です）。長さが違う値や非有限の値を含む行は除外されます（数えられ、VERIFYで可視になります）——スカラーの型強制失敗と同じ規律です。クエリベクトルも同じ形式を使います。デバッグ用に`csv:1.0,2.5,…`も受け付けます。

## 距離と結果

`cosine`（デフォルト。ベクトルは挿入時に正規化され、保存されるコピーは単位長です）、`l2`（二乗ユークリッド距離）、`ip`（内積の符号反転）。どの指標も「小さいほど近い」向きに揃えてあるので、シャードをまたぐ結果は昇順ソート1回でマージできます。`LIMIT`は1000で頭打ちです。カーソルはありません（ANNの深いページングはアンチパターンです）。

シャードごとのグラフは独立です（インデックスはキーに従い、シャードをまたぐ書き込み調整はゼロです）。クエリはファンアウトし、各シャードのtop-kを取り、マージします。

## 再現率：EFというノブ

`EF`（16〜4096、デフォルトはmax(4·LIMIT, 100)）はクエリ時のビーム幅であり、HNSWにおける再現率とレイテンシの正統なノブです。近い重複が密に固まった領域には、より広いビームが要ります（128次元・2万件のクラスタでの実測：EF 64 → recall@10は0.67、100 → 0.77、400 → ゲートラインの0.90以上）。組み込みでは`idx_knn(…, ef)`です（0でデフォルト）。

再現率は、厳密な総当たりの正解に対して、EF 400で0.90以上であることを[`bench/vectorgate.sh`](../../bench/vectorgate.sh)がゲートしています——現実的なコーパス幾何のうえで、です（128次元空間に埋め込まれた内在次元20の多様体。周囲空間で一様ランダムなベクトルは距離の集中を起こし、何も代表しません）。再現率が重要なら、ゲートがやっているのとまったく同じやり方で、総当たりのサンプルを使って**あなたの**コーパス上で計測し、インデックス単位ではなくクエリ単位でEFを調整してください。

## パラメータ、削除、再構築

`M`（ノードあたり・層あたりのリンク数、4〜64）と`EF`（構築時のビーム、16〜1024）は、**いったん作ったら不変**です。変えたければDROPして再CREATEすることになります。近傍選択には多様性ヒューリスティック（Malkov Alg. 4）を使い、外れた領域への橋渡しリンクを保存します。

削除はグラフのノードにtombstoneを立てます（ルーティングには使われ続け、マッチはしなくなります）。更新はtombstone + 再挿入です。`IDX.VERIFY`はvectors / bytes / tombstonesを報告し、死んだノードが30%を超えると`rebuild_recommended`のフラグを立てます。`IDX.REBUILD`は、シャードごとに生きているものを再挿入します（O(n · ef_construction · M · dim)で有界の仕事です——生きているキーはすべてHNSWの完全な挿入を通り直すので、近傍上限のMと次元数の両方が掛かってきます。答えは保存されます——e2eで表明済みです）。グラフは派生状態です。永続化されることは決してなく、再起動後にデータから再構築されます。サーバーではその再起動時の再構築はバックグラウンドのバックフィルであり、完了するまでクエリは`-INDEXBUILDING`を返します（[indexes.md](indexes.md)）。組み込みの再構築は同期的です。

## フィルタリングと、ハイブリッド検索

検索中のフィルタリングはありません（グラフの探索の内側に分割述語を持ち込むのは、クエリエンジンへの坂道です）。まずKNNし、それから`FIELDS`でhydrateして、クライアント側でフィルタしてください。

サーバーサイドで組み込まれている唯一の合成がこれです。同じコーパス上のANNインデックスとtextインデックスは、reciprocal-rank fusionで融合します。

```
IDX.QUERY HYBRID posts MATCH "rust storage" embs KNN <f32-le-blob>
    [LIMIT n] [RRFK k] [EF ef] [FIELDS f…]
```

——順位ベース（`Σ 1/(k + rank_i)`、`RRFK`のデフォルトは60）なので、BM25のスコアとベクトルの距離を互いに較正する必要はありません。MATCH側は[text-search.md](text-search.md)を参照してください。組み込みの呼び出し側は`idx_knn`と`idx_match`を走らせ、プロセス内で融合します。

**マルチモーダルの注意点。** 遠く隔たったモードからなるコーパス（モード間の隔たりがモード内の距離をはるかに上回る場合。たとえば、互いに素な2テナントの埋め込みを1つのインデックスに入れるなど）では、ナビゲーショングラフは劣化します——一方のモードへの遅い挿入がもう一方を発見できず、グラフが橋渡しに飢えるからです。現実の埋め込みコーパスは連続的な多様体なので、これは起きません。データが強くマルチモーダルなら、モードを別々にインデックスしてください（プレフィックスごとに1インデックス——安価で、互いに独立です）。

## 整合性

どのインデックスkindとも同じ包絡線です（[indexes.md](indexes.md)）。書き込みとそのグラフ更新は所有シャードの内側でアトミックです。シャードをまたぐクエリは、グローバルスナップショットなしにシャードごとのtop-kをマージします（SCANクラス）。カタログはデータディレクトリのサイドカーに永続化されます。グラフの**内容**は派生状態であり、再起動後に再構築されます。

## パフォーマンス

計測された包絡線です（領収書はbenchツリーにあります）。

- [`bench/vectorgate.sh`](../../bench/vectorgate.sh)は、実サーバーに対して1M × 128次元でKNN LIMIT 10のp95 < 30msをゲートし、同時にEF 400で再現率0.90以上が成立していることもゲートします（2つのclampを同時に——速いが間違った答えは通りません）。あわせて、メモリの式を実RSSの増分に対して（0.5〜1.5倍で）ゲートします。
- [`bench/PERF-LEDGER.md`](../../bench/PERF-LEDGER.md)が比較対決を記録しています。再現率を1.000に揃えたうえで、KNNは0.48 msで答えます（redis-stack 7.4.7のRediSearchのHNSWは同一コーパス上で0.79 ms）——**1.64倍の優位**です。

書き込み側のコストは標準的なインデックス税です（マッチするインデックス1つにつき、書き込みごとにフィールドのパース1回とグラフ挿入1回）。構築コストは`EF`（構築時のビーム）に比例して増えます。だからこそ、これは宣言時のパラメータなのです。

## サイジング

`bytes ≈ vectors × (dim×4 + 40) + links × 8 + vectors × 32`。`IDX.VERIFY`/`IDX.LIST`がライブで報告します。cosineは正規化されたコピーだけを保持します（単一のコピー——生のフィールドバイト列は行そのものの中に残ります）。1M × 1024次元でおよそ4.1 GiB、加えてリンク分です。ゲートはこの式を実RSSの増分に対して（0.5〜1.5倍で）検査します。

## 関連項目

- [indexes.md](indexes.md) — このkindが差し込まれるインデックスエンジン（宣言の文法、バックフィルの契約、整合性の包絡線）。
- [text-search.md](text-search.md) — BM25のkindと、`HYBRID`のもう半分。
- [verb-reference.md](../verb-reference.md) — すべての`IDX.*`形式の、生成された文法。
- [cookbook.md](cookbook.md) — 文脈の中の検索レシピ。
