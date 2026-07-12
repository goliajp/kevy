# 在 kevy 上设计应用

kevy 是一台**服务引擎**（serving engine）：给那些原本会把业务模型放进关系数据库、再在前面挡一层缓存的应用，做主数据存储。这一页是地图——引擎给你什么、有哪些边界法则、每项能力落在哪里。

从 RDS 迁过来的话，先读这一页，再照着 [cookbook](cookbook.md) 一条一条走。

## 七个平面

| 平面 | 承载什么 | 在哪 |
|---|---|---|
| **P0——操作** | 每个接口面上的每个操作：服务器（RESP）、embedded（进程内）、Lua、原子块、pipeline。同一张 OP_TABLE，接口面对等性由 CI 强制。 | docs/verb-reference.md |
| **P1——原子性与耐久性** | 单 shard 原子块、确定序的全 shard 块、appendfsync × 原子提交矩阵、逐块 fsync 屏障。 | docs/persistence.md |
| **P2——索引** | 声明式二级索引，四种 kind：`range`、`unique`、`text`（CJK bigram + BM25）、`ann`（HNSW）。构造即派生（写钩子维护，零漂移）、一跳补水（`FIELDS`）、backfill 重建。 | docs/indexes.md、docs/text-search.md、docs/vector-search.md |
| **P3——视图与代数** | 索引之上的具名组合（virtual / materialized top-K）、带完整 Redis 语义的 zset/set 代数。 | docs/views.md |
| **P4——流** | 带 `(generation, offset)` 游标的 CDC feed（内建的 outbox）、阻塞 pop、hash 字段级 TTL、快照读视图、embedded 只读 RESP listener。 | docs/cdc.md、docs/embedded-listener.md |
| **P5——证据** | 每条声明都实测并对账；崩溃一致性混沌 gate；3000 万键的混合栈 soak。 | bench/VALIDATION-LEDGER.md |
| **P6——可用性** | 带 acked-offset 真值与心跳的复制、计划内交接（`FAILOVER`）与崩溃切主（多数派选举、写权限只来自选举、丢弃分叉），以及可选购的一致性阶梯：`WAIT`、读己之写 token（`REPL.TOKEN` / `REPL.WAIT`）、有界陈旧（`-STALE`）、多数派租约写围栏。12 道可执行钳制（availgate）在 CI 里跑。 | docs/availability.md、docs/replication.md |

## 三条法则

这部宪法过去把 kevy 挡在“滑向一个更差的 RDS”之外，今后也会：

**法则 1——Redis 契约不可侵犯。** kevy 实现的每个 Redis 操作，行为与 Redis 完全一致。新体裁的能力从不改变既有 verb 的语义，从不把内部键空间泄漏进 `SCAN` / `KEYS` / `DBSIZE`，并且只活在自己的命令命名空间里（`IDX.*`、`VIEW.*`、`FEED.*`、`PREFIX.*`）。

**法则 2——只有属于引擎体裁的东西才准做超集。** 一项能力要进来，前提是它是派生数据，且生命周期能被引擎完整持有：声明 → 维护 → 校验 → 重建。索引、视图、feed、摘要都过了这道考试。存进服务器的应用逻辑没过。

**法则 3——RDS 视界。** 没有查询语言、没有 planner、没有 join、没有服务端校验、没有 trigger。引擎一旦开始**决定**怎么回答一个问题，而不是**执行一条声明过的访问路径**，它就是个 RDS——而且是个更差的。kevy 永久停在视界的这一侧。

## 想过，然后拒绝

下面每一条的需求都是真的，只是这个特性的形状不对。每一条都有对应的答案：

| 你可能会要 | 改用 |
|---|---|
| SQL / 查询 DSL | 显式索引 API + [cookbook](cookbook.md) |
| 查询 planner / 自动选索引 | `IDX.EXPLAIN`（仅供诊断） |
| Join | 一跳补水（`FIELDS`、`VIA`）+ 应用侧组装 |
| 外键 / 级联 | 原子块、前缀批量操作、CDC 消费者 |
| CHECK 约束 | 原子块内部的读（应用求值，引擎原子提交） |
| Schema / 有类型的列 | 字节是你的；类型在建索引时声明 |
| JSON-path 查询 | hash 字段**就是**列模型 |
| Trigger / 写路径 UDF | CDC 消费者——提交之后、解耦、可重放 |
| GROUP BY / 聚合管线 | 在你自己的模型里，于写入时维护聚合值 |
| 时间旅行 | 快照 + CDC 保留窗口（恢复点契约） |
| 事务性 outbox | 不需要——feed 就是 outbox |
| 没有索引的 WHERE | 刻意缺席：要么把访问路径建模出来，要么别上线 |

## 服务宪章（永久受 gate 约束的部分）

数字是棘轮，只升不降。现行的线（实测值在 `bench/VALIDATION-LEDGER.md`）：

- Redis 对等吞吐：12 角 perfgate，下限 = 基线 × 0.92。
- 补水后的行列表分页 p99 < 1ms；视图分页 < 1ms；穿过 index + view 钩子的写扇出 p99 < 200µs——全部是在一台扛着完整栈的服务器上。
- IDX.QUERY p99 < 2ms @ 100 万行；MATCH p95 < 20ms @ 100 万文档；KNN p95 < 30ms 且 recall@10 ≥ 0.90 @ 100 万 × 128 维。
- 崩溃诚实：写到一半 kill -9 → 重放 → 派生状态与一次全新重建完全一致。恢复点 = 快照 + `(gen, offset)`。
- 空目录不要钱：每个子系统都钳住一条零税线。

## 从哪开始

1. 把前缀和访问路径建模出来（[cookbook §1-2](cookbook.md)）。
2. 先批量导入，再声明索引（[迁移指南](migration.md)）。
3. 为热点列表组合视图（[views](views.md)）。
4. 把事件消费者接到 feed 上（[CDC](cdc.md)）。
5. 用 digest 校验，盯住 gate。
6. 一个节点不够用时，加副本，并在一致性阶梯上选一级站定（[availability](availability.md)）。
