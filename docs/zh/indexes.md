# 二级索引（`IDX.*` / `idx_*`）

kevy 可以在键空间的一个**前缀域**上维护声明式二级索引：该前缀下的每个 hash 键是一“行”，其中一个声明过的 hash 字段是被索引的值。索引**与每一次写入同步维护**（构造即派生——索引不可能与数据发生漂移，而 `IDX.VERIFY` 让这句话可被证伪），查询侧支持游标分页、双索引组合，以及可选的字段补水。

```
IDX.CREATE idx_age ON PREFIX user: FIELD age TYPE i64 KIND range
HSET user:42 age 31 name "……"
IDX.QUERY idx_age RANGE 18 30 LIMIT 100 FIELDS name
```

## 声明

`IDX.CREATE <name> ON PREFIX <p> FIELD <f> TYPE i64|f64|str KIND
range|unique [MAXMEM <bytes>]`

- **TYPE** 是一次标量强制转换：字段缺失或解析失败的行被**排除**（逐索引计数——`IDX.VERIFY` / `IDX.LIST` 会报 `coerce_failures`；这是声明式的围栏，不是运行期错误）。
- **KIND range** 服务 `RANGE min max` 扫描；**unique** 在此之上再加一道重复围栏（见下文）。
- **MAXMEM** 给索引的内存封顶：一次越过预算的构建会声明式地失败（查询回答 `-INDEXOVERBUDGET`），而不是无边界地涨下去。
- 最多 64 个索引。目录（catalog）持久化在数据目录的 sidecar 文件里；索引**内容**是派生状态——它从不进快照、也不写 AOF，重启后在后台重建（未就绪期间查询回答 `-INDEXBUILDING`；数据可用性从不等索引）。

## 查询

- `IDX.QUERY <name> RANGE <min> <max> | EQ <v> [LIMIT n] [CURSOR c]
  [FIELDS f…]` → `[next-cursor, rows]`。行是跨全部 shard 按 `(value, key)` 排序的；`FIELDS` 会在每一行所属 shard 上就地补上指定的 hash 字段（不需要第二次往返），并把行切换成嵌套的 `[key, value, fname, fval…]` 形态。
- `IDX.QUERY COMPOSE AND|OR <n1> <spec1> <n2> <spec2> …`——双索引组合，**按键排序**（两个值域不同），LIMIT / CURSOR / FIELDS 尾巴一样。AND / OR 逐 shard 求解（一个键只住在一个 shard 上，所以逐 shard 的集合代数在全局也成立）。
- `IDX.COUNT <name> RANGE|EQ …`——不物化键，直接计数。
- `IDX.VERIFY <name>`——汇总统计：entries、bytes、coerce_failures、duplicates。
- `IDX.LIST`——目录，加上每个索引的状态 / 条目数 / 字节数。
- 游标契约属于 SCAN 类：整趟遍历期间稳定存在的行**恰好**被看到一次；并发的插入 / 删除可能出现，也可能不出现。`"0"` = 起点 / 已耗尽。

## 唯一性是围栏，不是锁

`unique` 索引**不阻塞写入**——在写入时强制全局唯一，就意味着把跨 shard 的写串行化。它的做法是：重复项被计数（VERIFY / LIST 里的 `duplicates`），并且在 `EQ` 读取时以多命中的形式暴露出来。如果你需要的是硬唯一性，就在集群模式下用 `{hashtag}` 前缀把这个域钉到单个 shard 上，或者在 `MULTI` / `WATCH` 下做先查后写。

## Embedded

同一台引擎，类型化 API：`idx_create` / `idx_drop` / `idx_query` / `idx_count` / `idx_stats` / `idx_list`（值是 `IndexValue`，游标是 `IndexCursor`）。没有 `FIELDS` 补水——你人在进程内，字段直接用 `hget` 读。`idx_create` 同步构建，返回即代表索引可服务。

## 索引预算

**64 条索引，全局。** 不是每前缀、也不是每分片——整个 store 一共 64 条（`MAX_INDEXES`，`kevy-index/src/catalog.rs`）。

按字面读，这个数字挡住任何真实 schema：58 张表对 64 条索引看起来不可能，而一次迁移可能在算术上就卡住，还没来得及发现**那道算术本身是错的**。

**索引是一份稀缺的全局预算，而大多数访问路径根本不花它。** 父子导航属于链接键与 zset——`SMEMBERS order:1001:items` 不占任何索引槽，你自己维护的有序 zset 索引也不占（[cookbook §2](cookbook.md#2-一对多多对多)）。索引槽只花在链接键表达不了的东西上：

- **全局值范围**——"所有超过一万的发票"，跨全部行
- **文本检索**——`KIND text`
- **聚合**——`KIND agg`，写时 GROUP BY

一个按"每张表一条"读起来要 58 条索引的 schema，按"每种全局查询形状一条"读通常不到 20 条。如果你在逼近 64，该问的问题是：**它们里面有几条其实是披着索引外衣的父子导航。**

## 一致性与成本模型

- 一次写入和它引发的索引更新，在所属 shard 内是原子的（单 reactor 线程 / shard 锁）。跨 shard 查询逐 shard 归并，没有全局快照（SCAN 类，和 DBSIZE 同级）。
- **空目录的代价是每次写入一个不被走到的分支**（一次 Relaxed 原子读）。一旦声明了索引，落在被索引域里的写入，每命中一个索引就要付一次 hash 字段读 + 一次 B-tree 更新。
- 每个索引的内存 ≈ `rows × (value_width + avg_key_len + 48)` 字节（那个常数是逐条目的结构开销）。`IDX.LIST` 报告实测字节数；`bench/idxgate.sh` 钳住这条公式。

## 聚合 kind（`KIND agg`）——写入时 GROUP BY

```
IDX.CREATE ord_amt ON PREFIX ord: FIELD amount TYPE i64 KIND agg GROUPBY status
IDX.QUERY ord_amt GROUP paid                      → [count, sum, min, max, avg]
IDX.QUERY ord_amt GROUPS BY sum LIMIT 100         → ranked [group, count, sum, min, max]
```

这是引擎对 `SELECT g, COUNT(*), SUM(v) … GROUP BY g` 的回答：聚合值**在写路径上维护**（一条声明过的访问路径——绝不是查询期的全行扫描）。min / max 借助逐组的值多重集，在删除下仍保持精确；sum 用 f64 累加（精度边界见文档）；值强制转换失败、或分组字段缺失的行，被排除并计数（VERIFY 可见）。跨 shard 归并是精确的：count / sum 相加，极值取极。`GROUPS` 按 count / sum / max 降序或 min 升序排名，LIMIT ≤ 1000。

没有 HAVING、没有聚合表达式、没有近似 sketch——那是查询语言的斜坡。`GROUPS` 的结果拿回应用里过滤。

Embedded：`idx_create_agg(name, prefix, field, ty, group_by)` / `idx_group(name, g)` / `idx_groups(name, by, limit)`。

内存 ≈ `groups × (gkey+64) + distinct_values × 18 + rows × (key+10)`（常数是对着实测 RSS 校准出来的）；`bench/agggate.sh` 拿它跟真实 RSS 对钳，同时钳住 GROUP p99 < 1ms @ 100 万行 × 1 万组、GROUPS top-100 < 5ms、写入税 < 10%。
