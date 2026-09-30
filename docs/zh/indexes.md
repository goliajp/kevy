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
- 最多 64 个索引。目录（catalog）记录在日志和每份快照里，并随复制下发；索引**内容**是派生状态——它从不进快照、也不写 AOF，重启后在后台重建（未就绪期间查询回答 `-INDEXBUILDING`；数据可用性从不等索引）。

## 查询

- `IDX.QUERY <name> RANGE <min> <max> | EQ <v> [LIMIT n] [CURSOR c]
  [FIELDS f…]` → `[next-cursor, rows]`。行是跨全部 shard 按 `(value, key)` 排序的；`FIELDS` 会在每一行所属 shard 上就地补上指定的 hash 字段（不需要第二次往返），并把行切换成嵌套的 `[key, value, fname, fval…]` 形态。
- `IDX.QUERY COMPOSE AND|OR <n1> <spec1> <n2> <spec2> …`——双索引组合，**按键排序**（两个值域不同），LIMIT / CURSOR / FIELDS 尾巴一样。AND / OR 逐 shard 求解（一个键只住在一个 shard 上，所以逐 shard 的集合代数在全局也成立）。
- `IDX.COUNT <name> RANGE|EQ …`——不物化键，直接计数。
- **非标量 kind 用自己的词表回答 `VERIFY`。** `KIND agg` 答 `rows / bytes / excluded / groups`；`KIND text` 答 `docs / bytes / postings / tokens`；`KIND ann` 答 `vectors / bytes / tombstones / links / rebuild_recommended`。它们都**不打印** `drift` / `missing`——审计的那个问题（"这个条目所指的行还派生这个值吗"）适用于按行键的条目，而它们的条目是组、倒排项和图节点。（这些数字曾被贴着标量标签打印：一个健康的 3 文档 text 索引答过 `coerce_failures 7, duplicates 7`——那是它的 postings 与 token 数，却穿着完整性告警的名字。）
- **这对聚合的计数值意味着什么**：运行中的累计值**在运行时从不与键空间重算**，所以它与现实是否一致，靠的是每一条写路径都维护了它——而这件事 `IDX.VERIFY` 对这个 kind **证伪不了**。替它做这件事的是测试：`index_write_path_coverage` 在每个动词之后把组的计数与真实存活的行对账。
- `IDX.VERIFY <name>`——汇总统计：entries、bytes、coerce_failures、duplicates，外加**审计的两个方向**：`drift`（条目所指的行已经没了、不再能强制转换、或转换成了另一个值）在 `checked` 个条目上，以及 `missing`（前缀下能派生出值、却没有条目的行）。健康的索引上两者都应为零；**`missing` 是走索引自己的条目那一趟看不见的方向**。`kevy-cli --kevy doctor` 把这句话变成一个对所有已声明表的退出码，于是「应当为零」可以是一条 cron，而不是某个人记得去查的事（[table-migration.md](table-migration.md#8-让-verify-成为运维的一部分而不是迁移的一步)）。
- `IDX.LIST`——目录，加上每个索引的状态 / 条目数 / 字节数。
- 游标契约属于 SCAN 类：整趟遍历期间稳定存在的行**恰好**被看到一次；并发的插入 / 删除可能出现，也可能不出现。`"0"` = 起点 / 已耗尽。

## 唯一性是围栏，不是锁

`unique` 索引**不阻塞写入**——在写入时强制全局唯一，就意味着把跨 shard 的写串行化。它的做法是：重复项被计数（VERIFY / LIST 里的 `duplicates`），并且在 `EQ` 读取时以多命中的形式暴露出来。

**那个计数器是按 shard 的，读取不是。** `duplicates` 维护在每个 shard 各自的段里，所以它只在两行**落到同一个 shard** 时才看得见它们共享一个值——而键本来就哈希散布到各 shard，所以寻常情况恰恰是它们没落在一起。实测：同一对行，在单 shard 服务器上报 `duplicates 1`，在双 shard 上报 `duplicates 0`。做一个全局计数器需要在写路径上跨 shard 统计取值，而那正是这个 kind 存在所要避开的串行化——所以**永远有效的检测是 `EQ` 读的多命中**；`duplicates` 是提示，那里读到零**不等于**这个值是唯一的。如果你需要的是硬唯一性，就在集群模式下用 `{hashtag}` 前缀把这个域钉到单个 shard 上，或者在 `MULTI` / `WATCH` 下做先查后写。

**在嵌入式 API 里，`atomic()` 内部根本读不了索引**，所以 `KIND unique` 连乐观地参与这次检查都做不到。改用**认领键**：`u:<constraint>:<value>` 持有所有者的 id，在事务内部用 `get` 读、用 `set` 写。事务让「检查并认领」成为原子的——而那正是 `unique` 索引有意不提供的那个保证。

有一位用户就这样实现了 22 条唯一性约束，其中一条也没用 `KIND unique`；这个模式是成立的，但他们是靠自己发现这处缺口走到那里的，不是靠读到这里。


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

## 全局索引（`PARTITION global`）

索引默认是本地的：每个 shard 只索引自己持有的行，所以每次查询都要发给所有 shard，再在发起端把各页归并。**全局**索引按值切成若干分区，每个 shard 一个。行仍然留在它的键哈希到的 shard 上，它的索引条目放在它的值所落的分区里。

```
IDX.CREATE by_age ON PREFIX user: FIELD age TYPE i64 KIND range PARTITION global SPLIT 30 SPLIT 60
TABLE.DECLARE user PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range GLOBAL SPLIT AT 30 60
```

- **分裂点。** `SPLIT v`（每个点写一次，因为 `IDX.CREATE` 的选项成对出现）或 `GLOBAL SPLIT AT v…`，最多比 shard 数少一个。不写的话，每个 shard 把自己各行的值排好序，按秩切成每个分区 256 个等大的桶，每个桶交出它最大的值和行数；kevy 按行数合并。这样每个分裂点的误差最多是每个 shard 一个桶，最大分区和均值相差在 2/256（0.8%）以内。在没有行的时候创建的索引只有一个分区，直到 `IDX.REBUILD` 重新采样。同一个值的条目都在同一个分区里，所以一个值占的行比它应得的份额多时，它无法被拆开。`ORDERPATH … GLOBAL` 采样，或者接受 `TABLE.DESCRIBE` 写出的那种 `SPLIT AT`：`0x` 加上这条路径的顺序编码字节，因为那里的一个点横跨好几列。
- **读。** `EQ`，或者落在一个分区之内的 `RANGE`，只读一个 shard。按 `(value, key)` 顺序翻页时，依次走需要的分区，把各段直接拼接，不做 N 路归并。`IDX.COUNT` 和选择子句（`SORT`、`DISTINCT`、`FACET`、`OFFSET`）只发给范围覆盖到的分区。`IDX.EXPLAIN` 会写出是哪些。
- **写。** 改变了行的条目的写入，给条目所在分区发一条消息（条目换分区时两条），客户端的回复等分区应用完才返回：回复之后发出的读一定能看到这次写入。
- **`FIELDS` 取自 `VALUES`。** 分区持有的是条目，不是行，所以全局索引用它存下的列回答 `FIELDS`。没存的字段会被点名拒绝，`IDX.ADVISE` 会建议把它加进 `VALUES`。
- **种类与限制。** 只支持 `range` 和 `unique`。不能用于 `COMPOSE` 和视图（两者都要求行的条目和行在同一个 shard），不能用于窗口表，也不能用于嵌入式存储，都会点名拒绝。
- **建造。** 在每个 shard 都把自己已有行的条目发完之前，查询回答 `-INDEXBUILDING`；查询不会看到不完整的分区。
- **运维。** `IDX.LIST` 给每个索引报 `partitioning`，全局索引另外报 `partitions`、`max_entries`、`mean_entries`，倾斜程度就是后两者之比。`IDX.REBUILD <name>` 重新采样并重建。`IDX.VERIFY` 把每一行和它所在分区持有的条目逐一对账，所以 `drift` 和 `missing` 是精确值，全局唯一索引的 `duplicates` 覆盖整个键空间（本地唯一索引只看得到同一个 shard 内的重复）。分区数多于 shard 数时重启，会均匀保留一部分分裂点。
- **内存。** 条目本身的开销和本地索引一样；行所在的 shard 不再为全局索引按行保存任何东西：一次写入会带上这一行原来被索引的值，由这个值就能知道哪个分区存着它的条目。`IDX.LIST` 和 `IDX.VERIFY` 的 `bytes` 报告各分区实际占用的字节。

## 一致性与成本模型

- 一次写入和它引发的索引更新，在所属 shard 内是原子的（单 reactor 线程 / shard 锁）。跨 shard 查询逐 shard 归并，没有全局快照（SCAN 类，和 DBSIZE 同级）。
- **空目录的代价是每次写入一个不被走到的分支**（一次 Relaxed 原子读）。一旦声明了索引，落在被索引域里的写入，每命中一个索引就要付一次 hash 字段读 + 一次树的更新。写入改动一行之前，存储层会先记下这一行被索引的字段，所以不管这一行是被什么写的（命令、脚本内部的调用、事务、过期、淘汰、复制过来的帧），更新时都知道该删掉哪个旧条目。
- 每个索引的内存 ≈ `rows × ((value_len + handle_len + 3) / fill + 1)` 字节。索引是一棵 B+ 树，叶子 1,784 字节；每个条目占一个 10 字节的槽位，再加上排序键超出前 8 字节的部分。`i64` / `f64` 的 `value_len` 是 8，`str` 是字符串长度加 2；`handle_len` 是去掉索引前缀之后的键长，键全是数字时减半（向上取整）。`fill` 是叶子的填充率：建完索引或 `IDX.REBUILD` 之后是 1.0（这两者会把叶子压实），行按随机顺序写入之后是 0.6–0.7。在 125 万行、键为 `row:<n>` 的数据上实测：`i64` 索引压实时每行 15.9 字节，随机写入后 23–25 字节；值为 10 字节字符串的 `str` 索引分别是 20.2 和 30–38。`IDX.LIST` 和 `IDX.VERIFY` 报告叶子实际占用的字节；`bench/idxgate.sh` 拿它和服务器实测的 RSS 对账。
- 声明了 `VALUES` 的索引把这些值放在同一个条目里：每个值一个 1 字节的标记，后面跟着值本身，十进制数字串每位占半个字节，其他值按原样字节存。把这部分加到上面的条目里：一个短字符串加一个 10 位数字，实测每行多约 10 字节。`bytes` 已包含这一项。

## 聚合 kind（`KIND agg`）——写入时 GROUP BY

```
IDX.CREATE ord_amt ON PREFIX ord: FIELD amount TYPE i64 KIND agg GROUPBY status
IDX.QUERY ord_amt GROUP paid                      → [count, sum, min, max, avg]
IDX.QUERY ord_amt GROUPS BY sum LIMIT 100         → ranked [group, count, sum, min, max]
```

**`GROUPBY` 只取一个字段，而现实中 `GROUP BY` 的形状通常需要不止一个。** 按方向拆分的 `SUM(amount) GROUP BY month`，或者任何 `SUM(CASE WHEN …)`，都靠把条件**搬进分组键**来表达：在写入时实体化一个复合字段（`ym_dir = "2026-07:in"`），按它分组，再在应用侧把键拆开。条件聚合也是同一个做法——条件成为「你按什么分组」的一部分，而不是聚合本身的一部分。

这个惯用法事后看是显然的，初次接触时是看不见的：`KIND agg` 看上去像是在回答 `GROUP BY`，然后并不回答人们实际会写的那个形状。

这是引擎对 `SELECT g, COUNT(*), SUM(v) … GROUP BY g` 的回答：聚合值**在写路径上维护**（一条声明过的访问路径——绝不是查询期的全行扫描）。min / max 借助逐组的值多重集，在删除下仍保持精确；sum 用 f64 累加（精度边界见文档）；值强制转换失败、或分组字段缺失的行，被排除并计数（VERIFY 可见）。跨 shard 归并是精确的：count / sum 相加，极值取极。`GROUPS` 按 count / sum / max 降序或 min 升序排名，LIMIT ≤ 1000。

没有 HAVING、没有聚合表达式、没有近似 sketch——那是查询语言的斜坡。`GROUPS` 的结果拿回应用里过滤。

Embedded：`idx_create_agg(name, prefix, field, ty, group_by)` / `idx_group(name, g)` / `idx_groups(name, by, limit)`。

内存 ≈ `groups × (gkey+64) + distinct_values × 18 + rows × (key+10)`（常数是对着实测 RSS 校准出来的）；`bench/agggate.sh` 拿它跟真实 RSS 对钳，同时钳住 GROUP p99 < 1ms @ 100 万行 × 1 万组、GROUPS top-100 < 5ms、写入税 < 10%。
