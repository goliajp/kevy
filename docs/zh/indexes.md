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
- **`VERIFY` 审计不到的东西：聚合的计数值。** 对 `KIND agg`，一个条目是一个**组**而不是一行，所以"这个条目所指的行还派生这个值吗"根本不适用——无论计数对不对，`drift` 与 `missing` 都是零，而运行中的累计值**在运行时从不与键空间重算**。结构自身的算术有单测对着一个走查参照物验；它与键空间是否一致，靠的是每一条写路径都维护了它，而这件事 `IDX.VERIFY` 对这个 kind **证伪不了**。替它做这件事的是测试：`index_write_path_coverage` 在每个动词之后把组的计数与真实存活的行对账。
- `IDX.VERIFY <name>`——汇总统计：entries、bytes、coerce_failures、duplicates，外加**审计的两个方向**：`drift`（条目所指的行已经没了、不再能强制转换、或转换成了另一个值）在 `checked` 个条目上，以及 `missing`（前缀下能派生出值、却没有条目的行）。健康的索引上两者都应为零；**`missing` 是走索引自己的条目那一趟看不见的方向**。`kevy-cli doctor` 把这句话变成一个对所有已声明表的退出码，于是「应当为零」可以是一条 cron，而不是某个人记得去查的事（[table-migration.md](table-migration.md#8-让-verify-成为运维的一部分而不是迁移的一步)）。
- `IDX.LIST`——目录，加上每个索引的状态 / 条目数 / 字节数。
- 游标契约属于 SCAN 类：整趟遍历期间稳定存在的行**恰好**被看到一次；并发的插入 / 删除可能出现，也可能不出现。`"0"` = 起点 / 已耗尽。

## 唯一性是围栏，不是锁

`unique` 索引**不阻塞写入**——在写入时强制全局唯一，就意味着把跨 shard 的写串行化。它的做法是：重复项被计数（VERIFY / LIST 里的 `duplicates`），并且在 `EQ` 读取时以多命中的形式暴露出来。

**那个计数器是按 shard 的，读取不是。** `duplicates` 维护在每个 shard 各自的段里，所以它只在两行**落到同一个 shard** 时才看得见它们共享一个值——而键本来就哈希散布到各 shard，所以寻常情况恰恰是它们没落在一起。实测：同一对行，在单 shard 服务器上报 `duplicates 1`，在双 shard 上报 `duplicates 0`。做一个全局计数器需要在写路径上跨 shard 统计取值，而那正是这个 kind 存在所要避开的串行化——所以**永远有效的检测是 `EQ` 读的多命中**；`duplicates` 是提示，那里读到零**不等于**这个值是唯一的。如果你需要的是硬唯一性，就在集群模式下用 `{hashtag}` 前缀把这个域钉到单个 shard 上，或者在 `MULTI` / `WATCH` 下做先查后写。

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
