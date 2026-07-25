# 视图（`VIEW.*` / `view_*`）

视图是**若干声明过的索引的具名组合**——一棵由索引形状构成的 AND / OR / DIFF 树，外加一个排序索引——可以作为一个整体来查询，要么每次查询时求值（**virtual**），要么在每次写入时增量维护（**materialized**，可选择用 top-K 封顶）。视图是引擎对“热点列表”查询的回答——`WHERE state = 'ready' AND pri BETWEEN 0 AND 100 ORDER BY pri DESC LIMIT 10`——把它做成一条声明过的访问路径，而不是查询期的扫描。

```
IDX.CREATE j_pri  ON PREFIX job: FIELD pri  TYPE i64 KIND range
IDX.CREATE j_state ON PREFIX job: FIELD state TYPE str KIND range

VIEW.CREATE ready_jobs
    QUERY ( AND j_pri RANGE 0 100 j_state EQ ready )
    ORDER BY j_pri DESC
    MODE materialized TOPK 100
VIEW.QUERY ready_jobs LIMIT 10
```

## 快速上手（服务器）

视图要组合的一切都必须先存在——叶子和 ORDER BY 索引，都是在同一个前缀域上声明过的 `IDX.*` 索引（[indexes.md](indexes.md)）：

```console
kevy-cli -p 6004 IDX.CREATE j_pri ON PREFIX job: FIELD pri TYPE i64 KIND range
kevy-cli -p 6004 IDX.CREATE j_state ON PREFIX job: FIELD state TYPE str KIND range
kevy-cli -p 6004 VIEW.CREATE ready_jobs QUERY ( AND j_pri RANGE 0 100 j_state EQ ready ) ORDER BY j_pri DESC MODE materialized TOPK 100
kevy-cli -p 6004 HSET job:1 pri 10 state ready
kevy-cli -p 6004 HSET job:2 pri 99 state blocked
kevy-cli -p 6004 VIEW.QUERY ready_jobs LIMIT 10
```

回复按视图顺序分页给出 `key, order-value` 对，并带一个可续读的 `CURSOR`。树的语法（一行，带括号）：

```
tree     = '(' AND|OR|DIFF sub sub ')' | leaf
leaf     = <index> RANGE <min> <max> | <index> EQ <value>
```

配套 verb：`VIEW.LIST`（目录 + 模式 + 形状）、`VIEW.EXPLAIN name`（带逐叶子基数的树）、`VIEW.VERIFY name`（成员数 / 字节数 / 排序排除数——**字节数和排序排除数只对 materialized 有意义**；virtual 视图什么都不存，两项都报 0，而校验一个 virtual 视图要付一次全新的 `eval_tree`，这是 virtual 视图唯一比较贵的地方）、`VIEW.REBUILD name`（强制重建 materialized 内容——保持答案不变）、`VIEW.DROP name`。

## 三条结构规则

1. **组件是具名索引。** 叶子携带一个形状（`RANGE min max` 或 `EQ v`，在 CREATE 时强制转换成被引用索引的类型）；视图层自己不持有任何谓词。树深 ≤ 3、叶子 ≤ 4 个；`AND` / `OR` 可能被引擎重排，`DIFF` 固定是左减右。
2. **视图只存成员关系和顺序**——绝不存字段值。`ORDER BY <index>` 提供排序键；不在排序索引里的行被排除（有计数，`VIEW.VERIFY` 可见）。
3. **补水是解引用，不是查询。** `VIA <template>`（例如 `user:{key.1}`；`{key}` = 成员键，`{key.N}` = 它按 `:` 切分后的第 N 段）把每个成员映射到一个目标键；`VIEW.QUERY … FIELDS f…` 在目标所属的 shard 上、通过第二次内部扇出读取那些字段。目标不存在 = 行字段为 nil。目标不接受任何谓词。

## 模式与成本模型

- **Virtual**——查询时求值：按顺序流式扫过 ORDER 索引，对每个候选探测树的成员关系。一页 LIMIT-100 的代价是 O(limit / 选择率) 次探测，而不是 O(成员数)。永远新鲜；零写入成本。它的失效形态是“高选择性的树压在一个庞大的 ORDER 域上”（每吐出一行要探测很多候选）——那种形状想要的是物化。
- **Materialized**——逐 shard 的有序成员集，在维护索引的同一个写钩子里更新（每次写入、每个被引用的索引探测一次，所有视图共享）。`TOPK k` 把它约束在 `k + k/4`，从视图最差的那一端淘汰；一个比当前最差者还差的非成员，只用一次比较就被拒绝——[`bench/viewgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/viewgate.sh) 实测的稳态写入税，在 3 个索引 + 4 个 top-K 视图下约为 2%（gate 钳在 < 15%）。收缩到 `k` 以下会安排一次逐 shard 的本地重建（下一个 tick）。不设上限的 materialized 视图，每笔受影响的写入付 O(log 成员数)。
  - `VIEW.CREATE` 刚做完时，预期会有一个短暂的稳定窗口（新建的 top-K 集合在淘汰阈值稳定下来之前，第一波写入会跑得慢一些）。

怎么选：高写入域上的一个看板 top-100 想要 `MODE materialized TOPK 100`（内存有界，非争夺者被 O(1) 拒绝）；一个中等规模域上偶尔翻页的报表想要 `MODE virtual`（零写入税，永远新鲜）。`VIEW.EXPLAIN` 给出逐叶子的基数——那就是这次判断所需的选择率数据。

### DIFF：声明式的反连接

`DIFF` 是左减右，且**不满足交换律**——它是引擎唯一不会重排其子节点的树节点。经典用法是“合格但未被排除”：

```
VIEW.CREATE assignable
    QUERY ( DIFF j_pri RANGE 0 100 j_state EQ quarantined )
    ORDER BY j_pri DESC
    MODE virtual
```

——工作优先级带里的每一个 job，减去被隔离的那些。

用 SQL 的话说，这就是 `WHERE … AND NOT EXISTS (…)` 那个形状——只不过做成了一条声明过的访问路径，而不是每次查询时的子查询（[rds-workloads.md](rds-workloads.md) 映射了这套词汇的其余部分）。

## 一致性

与索引同一个信封（[indexes.md](indexes.md)）：与触发它的那次写入在所属 shard 内原子，跨 shard 归并时没有全局快照（SCAN 类）。

- 只要有任何一个被引用的索引还在 backfill，查询就回答 `-INDEXBUILDING`（一个不完整的索引会悄悄谎报成员关系）——重试纪律与索引查询相同。
- `VIEW.REBUILD` 保持答案不变（e2e 套件里有断言）；`VIEW.VERIFY` 让漂移可被证伪（成员数 / 字节数 / 排序排除数）。
- 视图目录持久化在数据目录的 sidecar 文件里；materialized 的**内容**是派生状态——重启后重建，从不进快照。

## Embedded

类型化 API，在 `index` 这个 cargo feature 后面（默认开启）——树作为一个值传进去，进程内没有文本语法，而且每个调用都返回 `KevyResult`（4.0 的单一错误货币）：

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

- `view_create(name, tree, order_by, desc, mode)` 同步构建；每一个被引用的索引（叶子 + ORDER BY）都必须已经声明过（否则是 `KevyError::InvalidInput`）。
- `view_query(name, after, limit)` 分页给出 `(key, order_value)` 行；返回的游标以排他方式续读（DESC 视图从大的那一端开始翻）。`view_count` / `view_list` / `view_drop` 补齐整个接口面。
- 没有 `VIA` / `FIELDS`——进程内的调用方自己解引用，用 `hget` 直接读字段。

## 性能

实测信封——钳制住在 [`bench/viewgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/viewgate.sh) 里，对着一台真服务器在 100 万行上跑：

- Virtual `VIEW.QUERY` p99 < 3ms（两组件的树）。
- Materialized `VIEW.QUERY` p99 < 2ms（这是索引读那条线——一页 materialized 就是一次有序集合读）。
- 3 个索引 + 4 个 materialized top-K 视图下的写入税，相比只有索引的同一负载 < 15%（稳态实测约 2%）。
- 逐成员的内存落在下面那条公式的 ±20% 之内。

逐成员内存 ≈ `order_value_width + key_len + 48` 字节，由 `VIEW.VERIFY` 实时报告。

## 另见

- [indexes.md](indexes.md)——视图所组合的那些索引（声明、backfill，以及视图继承下来的一致性信封）。
- [rds-workloads.md](rds-workloads.md)——视图在 SQL 到 kevy 映射里所处的位置。
- [verb-reference.md](../verb-reference.md)——每一种 `VIEW.*` 形态的生成式语法参考。
- [cookbook.md](cookbook.md)——放在上下文里的视图菜谱。
