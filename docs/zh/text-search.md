# 全文检索（`KIND text`）

text 是索引引擎的第三种 kind：像声明任何索引一样声明它，字段的原始字节就会被切成词元、进入逐 shard 的倒排段，并与每一次写入同步维护（零漂移，与 `range` / `unique` 同一套“构造即派生”的纪律）。没有单独的搜索服务器，没有摄取管线，也没有分析器配置——一个 verb 声明索引，此后的每一次写入都让它保持精确。

```
IDX.CREATE posts ON PREFIX post: FIELD body TYPE str KIND text
IDX.QUERY posts MATCH "rust 全文检索" LIMIT 10 [FIELDS title body]
```

## 快速上手（服务器）

行就是声明前缀下的 hash 键，被索引的值是一个声明过的字段——和 `range` 索引完全一样（[indexes.md](indexes.md)）：

```console
kevy-cli -p 6004 IDX.CREATE posts ON PREFIX post: FIELD body TYPE str KIND text
kevy-cli -p 6004 HSET post:1 title "intro" body "kevy is a pure-Rust key-value store"
kevy-cli -p 6004 HSET post:2 title "search" body "dictionary-free CJK full-text search"
kevy-cli -p 6004 IDX.QUERY posts MATCH "full-text rust" LIMIT 10
```

回复是一个扁平数组，内容是 `key, score` 对，最佳者在前。加上 `FIELDS`，可以在同一次调用里、在每个命中所属的 shard 上补上指定的 hash 字段（不需要第二次往返）：

```console
kevy-cli -p 6004 IDX.QUERY posts MATCH "search" LIMIT 10 FIELDS title
```

配套 verb 在 text 索引上的用法和其他 kind 一样：

- `IDX.EXPLAIN posts MATCH "full-text rust"`——只解析、不执行：kind、构建状态、估算行数，以及计划行。
- `IDX.VERIFY posts` / `IDX.LIST`——实时的 entries / bytes / postings / token 统计。
- `IDX.DROP posts`——删掉这条声明（目录变更，落在 sidecar 文件里）。

`MATCH` 接受 `LIMIT`（≤ 1000）和 `FIELDS`；没有 `CURSOR` 形态（原因见“匹配与排名”）。

## 快速上手（embedded）

同一台引擎，在进程内。text kind 在 `text` 这个 cargo feature 后面（默认开启；编译掉之后，用 `IndexKind::Text` 调 `idx_create` 会回答 `KevyError::Unsupported`）：

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

`idx_match` 返回 `KevyResult<Vec<(Vec<u8>, f64)>>`；索引不存在时是 `KevyError::NotFound`。embedded 侧没有 `FIELDS` 补水——你人在进程内，字段用 `hget` 读。embedded 的构建是同步的：`idx_create` 返回即代表索引可服务（没有 `-INDEXBUILDING` 阶段要轮询）。

## 分词（无词典）

- 拉丁 / 字母数字连续段 → 小写化的词元（长度 ≥ 2）。
- CJK（统一表意文字、假名、谚文）→ **相邻 bigram**——「全文检索」索引成 全文 / 文检 / 检索，于是任意两字的子串查询都无需词典即可命中。孤立的单个 CJK 字符输出它自己。
- 词元绝不跨越书写系统边界；查询用同一套规则分词。

bigram 方案就是 CJK 故事的全部：没有词典要随包发布，没有词典会过期，混合书写系统的文档（`"Rust 入门"`）两半各按自己的规则入索引。代价是单字 CJK 查询的召回噪声（它们只能命中孤立单字的那些输出）——请用两个字以上来查。

## 匹配与排名

`MATCH` 是**查询词元之间的 OR 语义，按 BM25 排名**（k1=1.2、b=0.75、非负 idf 变体）。命中更多（且更罕见）查询词的文档排得更高；词频饱和；长文档被归一化压低。

两处声明在明处的近似：

- **分数是 shard 局部的。** df / avgdl 来自每个 shard 自己的语料（全局统计要求跨 shard 的写协调）。在哈希分片的键之下，各 shard 的统计量会收敛，归并后的排名是稳定的；不同 shard 来的分数彼此可比，但与单一语料上跑出来的结果不完全相同。
- **没有游标。** BM25 的深分页是反模式（第 N 页要求把它上面的一切重新打分）；`LIMIT` 封顶在 1000。

那一层表面的每个子句，如今都会执行：

| 子句 | 它做什么 |
|---|---|
| `IN <field…>` | 只在这些字段内打分——一次字段范围内的 BM25（自己的词频、自己的长度），而不是对整篇文档得分做过滤 |
| `FILTER <field> RANGE\|EQ …` | 对已保存值的非打分谓词；在 top-K 之前施加，所以排在第 40 位的合格文档仍能进入 `LIMIT 10` 的那一页 |
| `SORT <field> ASC\|DESC` | 按已保存的值而不是按得分选取；没有该值的文档在两个方向上都排在最后 |
| `DISTINCT <field>` | 每个值至多一条命中，取其组内最好的那条；折叠发生在选取过程中，所以这一页仍然装满 `LIMIT` 行 |
| `FACET <field…>` | 对**整个匹配集**而不是这一页做值计数；`FILTER` 会收窄它们，`DISTINCT` 不会 |
| `HIGHLIGHT [field…]` | 按字段返回匹配词的字节区间 |
| `TYPO 0\|1\|2` | 对裸词的编辑距离容忍度（短语与前缀仍然精确） |
| `LIMIT n` / `OFFSET m` / `FIELDS f…` | 页大小、页偏移、水合 |

`"带引号的短语"` 和 `word*` 前缀写在 `MATCH` 文本本身里。`FILTER`、`SORT`、`DISTINCT`、`FACET` 读取索引在 `VALUES` 声明时保存下来的字段；指名一个它没保存的字段是错误，而那个错误会把它保存了哪些字段告诉你。

一个受理却被忽略的子句会比报错更糟——被丢掉的 `FILTER` 返回未经过滤的行，那是一个穿着成功回复外衣的错误答案。所以这些关键字在其中任何一个能工作之前，就已经被冻结并按名字拒绝了。

（这推翻了此前的一条边界。本页的旧版本说短语查询与布尔查询刻意不在范围内——“如果你需要这些，你描述的是一个搜索引擎”。对一个止步于排序查找的 text kind 来说那是对的，而目标变了。）

## 混合检索（BM25 + KNN）

同一语料上的一个 text 索引和一个 ANN 索引，可以在服务端用倒数排名融合（reciprocal-rank fusion）合起来：

```
IDX.QUERY HYBRID posts MATCH "rust storage" embs KNN <f32-le-blob>
    [LIMIT n] [RRFK k] [EF ef] [FIELDS f…]
```

每个命中的融合分数是两份结果列表上的 `Σ 1/(k + rank_i)`（`RRFK` 默认 60——标准的融合常数；用的是排名位次而非原始分数，所以 BM25 和距离之间不需要相互校准）。KNN 那一半见 [vector-search.md](vector-search.md)。HYBRID 是服务器 verb；embedded 调用方自己跑 `idx_match` + `idx_knn`，在进程内融合。

## 生命周期与一致性

与每一种索引 kind 同一个信封（[indexes.md](indexes.md)）：

- 一次写入和它引发的段更新，在所属 shard 内是**原子的**（单 reactor 线程 / shard 锁）。一次更新精确地移除旧文档的词元、插入新的——为此文档会保留自己的原文，所以不存在墓碑漂移。
- 声明字段缺失的行，只是不贡献词元；`TYPE str` 的 text 字段没有强制转换失败一说。
- 跨 shard 查询归并逐 shard 的 top-K，没有全局快照（SCAN 类，和 `DBSIZE` 同级）。
- **服务器上的 backfill 是异步的**：在活跃键空间上 `IDX.CREATE` 之后，或者重启之后，查询在重建完成前一律回答 `-INDEXBUILDING`（数据可用性从不等索引构建；轮询或重试即可——见 [indexes.md](indexes.md)）。embedded 的构建是同步的。
- 倒排段是**派生状态**——从不进快照、从不写 AOF，重启后从数据重建。
- `IDX.CREATE` 上的 `MAXMEM` 给段的内存封顶；越过预算的构建会声明式地失败（查询回答 `-INDEXOVERBUDGET`），而不是无边界地涨下去。

## 性能

Top-K 求值用 MaxScore 剪枝（最罕见的词优先；常见词的列表一旦无法再把新文档抬进 top K，就只在候选上逐个探测）。倒排表是 doc-id 列表，做了两重 impact 排序：tf 桶降序，桶内按稀疏的 log2 dl 带升序——单词查询（没有第二个列表可供剪枝）会精确地停在第一个下界打不过第 K 名下限的带上，于是即便最常见的词，在 20 万文档上也约 0.1ms 应答（做全倒排扫描时约为 6ms）。只有一条 posting 的词元（Zipf 长尾）内联存放，不占堆。

实测信封（凭据在 bench 目录）：

- [`bench/textgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/textgate.sh) 对着一台真服务器，钳住 100 万篇混合书写系统文档（每篇约 100 字节）上的 `MATCH` p95 < 20ms，外加内存公式与真实 RSS 增长的对钳。它跑在与 CI 相邻的 release 检查里——这些数字是钳制，不是愿望。
- [`bench/PERF-LEDGER.md`](https://github.com/goliajp/kevy/blob/develop/bench/PERF-LEDGER.md) 记录了对打结果：在同一语料上，BM25 top-10 的 qps 比 RediSearch 的 `FT.SEARCH` 高 21%，p95 打平。

写入侧就是标准的索引税：每次写入、每命中一个索引，付一次 hash 字段读加一次段更新；空目录的代价是每次写入一个不被走到的分支。

## 定容

`bytes ≈ Σ_token (token_len + 48) + postings × 64 + Σ_doc (key_len + text_len + 72)`（文档保留原文，好让更新能精确移除自己的词元），由 `IDX.VERIFY` / `IDX.LIST` 实时报告（text kind 报 entries / bytes / postings / tokens）。`bench/textgate.sh` 拿这条公式跟真实 RSS 增长对钳。

重建走标准的 backfill 骨架（服务器上按 tick 增量推进，embedded 侧同步完成）。

## 另见

- [indexes.md](indexes.md)——这种 kind 插进去的那台索引引擎（声明语法、游标契约、一致性信封）。
- [vector-search.md](vector-search.md)——ANN kind，以及 `HYBRID` 的另一半。
- [verb-reference.md](../verb-reference.md)——每一种 `IDX.*` 形态的生成式语法参考。
- [cookbook.md](cookbook.md)——放在上下文里的全文检索菜谱。
