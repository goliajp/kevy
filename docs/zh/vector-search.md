# 向量检索（`KIND ann`）

ANN 是索引引擎的第四种 kind：像声明任何索引一样声明它，字段的原始字节就会被解析成 f32 向量，进入逐 shard 的 HNSW 图，并与每一次写入同步维护。没有旁挂的向量数据库，也没有摄取作业：embedding 和记录的其余部分住在同一个 hash 行里，图不可能与数据发生漂移。

```
IDX.CREATE embs ON PREFIX doc: FIELD v TYPE vector KIND ann DIM 768
    [DISTANCE cosine|l2|ip] [M 16] [EF 200]
IDX.QUERY embs KNN <f32-le-blob> LIMIT 10 [EF 400] [FIELDS title]
IDX.REBUILD embs
```

## 快速上手（服务器）

行就是声明前缀下的 hash 键，被索引的值是一个声明过的字段，里面装着向量的原始字节：

```console
kevy-cli -p 6004 IDX.CREATE embs ON PREFIX doc: FIELD v TYPE vector KIND ann DIM 4
kevy-cli -p 6004 HSET doc:1 title "intro" v "csv:0.1,0.2,0.3,0.4"
kevy-cli -p 6004 HSET doc:2 title "search" v "csv:0.4,0.3,0.2,0.1"
kevy-cli -p 6004 IDX.QUERY embs KNN "csv:0.1,0.2,0.3,0.4" LIMIT 2 FIELDS title
```

回复是升序的 `key, distance` 对（最近的在前）；`FIELDS` 在同一次调用里、在每个命中所属的 shard 上补上 hash 字段。生产环境里，向量值是你的客户端库写进去的二进制 blob（`dim × 4` 字节的小端 f32）；上面那个 `csv:` 形态是调试用的便利。

配套 verb 和其他 kind 一样：`IDX.EXPLAIN embs KNN …`（只解析加出计划，不执行）、`IDX.VERIFY` / `IDX.LIST`（实时统计）、`IDX.DROP`。`IDX.REBUILD` 是 ANN 专有的——见“删除与重建”。

## 快速上手（embedded）

ANN kind 在 `vector` 这个 cargo feature 后面（默认开启；编译掉之后回答 `KevyError::Unsupported`）：

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

`idx_knn` 返回 `KevyResult<Vec<(Vec<u8>, f32)>>`（`k` 夹到 1..=1000）；索引不存在时是 `KevyError::NotFound`，`AnnSpec` 非法（dim 为 0、距离标签未知）时是 `KevyError::InvalidInput`。embedded 的构建是同步的——`idx_create_ann` 返回即代表索引可服务。进程内没有 `FIELDS` 补水：字段用 `hget` 读。

## 线格式

一个向量字段装着 `dim × 4` 字节的小端 f32（RediSearch 的约定）。长度不对或含非有限值的行会被排除（有计数，VERIFY 里可见）——和标量强制转换失败同一套纪律。查询向量用同样的格式；调试时也接受 `csv:1.0,2.5,…`。

## 距离与结果

`cosine`（默认；向量在插入时归一化——存的那份副本是单位长度）、`l2`（欧氏距离的平方）、`ip`（内积取负）。每一种度量都朝“越小越近”定向，所以跨 shard 的结果只需一次升序排序就能归并。`LIMIT` 封顶 1000；没有游标（ANN 的深分页是反模式）。

逐 shard 的图彼此独立（索引跟着键走，零跨 shard 写协调）；一次查询扇出去，各取每个 shard 的 top-k，再归并。

## 召回：EF 这个旋钮

`EF`（16-4096，默认 `max(4·LIMIT, 100)`）是查询期的 beam 宽度——HNSW 的标准召回 / 延迟旋钮。稠密的近重复区域需要更宽的 beam（在 128 维的 2 万点簇上实测：EF 64 → recall@10 为 0.67，100 → 0.77，400 → 达到 ≥ 0.90 的 gate 线）。embedded 侧是 `idx_knn(…, ef)`（0 = 默认）。

召回率由 [`bench/vectorgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/vectorgate.sh) 对着精确暴力搜索的 ground truth，在 EF 400 处钳到 ≥ 0.90——用的是一个现实的语料几何（128 维空间里一个本征维度为 20 的流形；在环境空间里均匀随机的向量会受距离集中之苦，什么也代表不了）。如果召回率对你要紧，就照着 gate 那样，用暴力搜索的采样在**你自己的**语料上量一遍，并且按查询而不是按索引去调 EF。

## 参数、删除、重建

`M`（每节点每层的链接数，4-64）和 `EF`（构建期 beam，16-1024）**一经创建即不可变**——要改就得 DROP 之后重新 CREATE。邻居选择用的是多样性启发式（Malkov 算法 4），它会保住通往离群区域的桥接链接。

删除会给图节点打墓碑（它继续参与路由，但不再被匹配）；更新则是打墓碑加重新插入。`IDX.VERIFY` 报告 vectors / bytes / tombstones，死节点超过 30% 时置起 `rebuild_recommended`；`IDX.REBUILD` 逐 shard 把活着的重新插一遍（有界的 O(n · ef_construction · M · dim) 工作量——每个活键都要重新走一遍完整的 HNSW 插入，所以邻居上限 M 和维度都会乘进来，且保持答案不变——这一点在 e2e 里有断言）。图是派生状态：从不持久化，重启后从数据重建。在服务器上，那次重启重建是一个后台 backfill——完成之前查询回答 `-INDEXBUILDING`（[indexes.md](indexes.md)）；embedded 的重建是同步的。

## 过滤，以及混合检索

没有“搜索中过滤”（在图游走内部放分区谓词，是查询引擎的斜坡）：先 KNN，再用 `FIELDS` 补水，然后在客户端侧过滤。

唯一内建的服务端组合是这个：同一语料上的一个 ANN 索引和一个 text 索引，用倒数排名融合合起来——

```
IDX.QUERY HYBRID posts MATCH "rust storage" embs KNN <f32-le-blob>
    [LIMIT n] [RRFK k] [EF ef] [FIELDS f…]
```

——基于排名（`Σ 1/(k + rank_i)`，`RRFK` 默认 60），所以 BM25 分数和向量距离之间不需要相互校准。MATCH 那一半见 [text-search.md](text-search.md)。embedded 调用方自己跑 `idx_knn` + `idx_match`，在进程内融合。

**多模态告诫**：导航图在“由彼此远离的模态构成”的语料上会退化（模态间的间隙远大于模态内的距离，例如把两个互不相干的租户的 embedding 塞进一个索引）——某个模态里较晚插入的点发现不了另一个模态，图因此缺桥。真实的 embedding 语料是连续流形，不会触发这个问题；如果你的数据强烈多模态，就把各模态分开建索引（一个前缀一个索引——它们既便宜又互相独立）。

## 一致性

与每一种索引 kind 同一个信封（[indexes.md](indexes.md)）：一次写入和它引发的图更新，在所属 shard 内是原子的；跨 shard 查询归并逐 shard 的 top-k，没有全局快照（SCAN 类）。目录持久化在数据目录的 sidecar 文件里；图的**内容**是派生状态，重启后重建。

## 性能

实测信封（凭据在 bench 目录）：

- [`bench/vectorgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/vectorgate.sh) 对着一台真服务器，在 100 万 × 128 维上钳住 KNN LIMIT 10 的 p95 < 30ms，同时召回率在 EF 400 处 ≥ 0.90（两道钳制同时成立——一个又快又错的答案是过不了的），外加内存公式与真实 RSS 增长的对钳（0.5-1.5×）。
- [`bench/PERF-LEDGER.md`](https://github.com/goliajp/kevy/blob/develop/bench/PERF-LEDGER.md) 记录了对打结果：在同一语料上召回率对齐于 1.000 时，KNN 用 0.48 ms 应答，RediSearch 的 HNSW 用 0.79 ms——**领先 1.64 倍**。

写入侧的成本是标准的索引税（每次写入、每命中一个索引，付一次字段解析加一次图插入）；构建成本随 `EF`（构建期 beam）增长，这正是它作为声明期参数存在的原因。

## 定容

`bytes ≈ vectors × (dim×4 + 40) + links × 8 + vectors × 32`，由 `IDX.VERIFY` / `IDX.LIST` 实时报告。cosine 只保留归一化后的那一份（单份副本——原始字段字节仍然留在行里）。100 万 × 1024 维约 4.1 GiB，另加链接开销。gate 拿这条公式跟真实 RSS 增长对钳（0.5-1.5×）。

## 另见

- [indexes.md](indexes.md)——这种 kind 插进去的那台索引引擎（声明语法、backfill 契约、一致性信封）。
- [text-search.md](text-search.md)——BM25 kind，以及 `HYBRID` 的另一半。
- [verb-reference.md](../verb-reference.md)——每一种 `IDX.*` 形态的生成式语法参考。
- [cookbook.md](cookbook.md)——放在上下文里的检索菜谱。
