# 在 kevy 上跑 RDS 负载

你在 MySQL 或 PostgreSQL 上跑着一个业务负载,正在考虑把它 —— 整体或部分地 —— 搬到 kevy 上。本页是**参考矩阵**:每一种 SQL 层面的构造、在 kevy 里由什么承载、精确的 verb,以及语义差异。凡是 kevy 拒绝的构造,都直白地写明拒绝,并给出被覆盖的替代方案。配套的 [cookbook](../cookbook.md) 把这些行变成可运行的配方;[designing-on-kevy](../designing-on-kevy.md) 是引擎本身的一页地图。

## 分工

RDS 让你先写数据、以后再决定访问路径 —— 查询计划器在查询时把任何 `WHERE` 子句变成*某个*计划。kevy 把这件事倒过来:**你在写入时声明访问路径**(索引、聚合、视图),引擎随每次写入同步维护它们(按构造派生,零漂移),而服务端查询是微秒级的查找,永远不是计划搜索。这笔交易是明码标价的:

- 你放弃的:对未声明路径的临时查询。一个没有匹配索引的 `WHERE` 在 kevy 上不是慢 —— 它是*按设计不存在*(要么把访问路径建模出来,要么别上线这个查询)。
- 你得到的:可预测的服务延迟(单台服务器扛整套栈时,水合页面 p99 < 1ms、经索引+视图钩子的写扇出 p99 < 200µs —— 见 [designing-on-kevy](../designing-on-kevy.md) 里有 gate 把守的数字),以及远超磁盘 B-tree 引擎的 Redis 同级裸吞吐(见[容量估算](#容量估算与运维差异))。

这条边界是章程,不是待办:没有查询语言、没有计划器、没有 join、没有服务端校验 DSL、没有触发器("Law 3",[designing-on-kevy](../designing-on-kevy.md))。下文的一切都是把 SQL 构造映射到这个固定接口面上。

## 表、行、列

| RDS | kevy |
|---|---|
| 表 | 键前缀(`user:`)|
| 行 | 前缀下的 hash(`user:42`)|
| 列 | hash 字段 |
| PRIMARY KEY | 键本身 |
| NULL | 缺失的字段(绝不用哨兵字符串)|

```
HSET user:42 name ada email ada@example.com age 36
HGETALL user:42                # SELECT * WHERE id = 42
HGET user:42 email             # SELECT email WHERE id = 42
HMGET user:42 name age         # SELECT name, age WHERE id = 42
```

`HGET` 缺失字段返回 nil —— 这就是 NULL 语义,而索引规格把缺失字段视为"该行被排除"(会被计数,在 `IDX.VERIFY` 里可见)。

### 类型系统

kevy 存储的是字节;类型在**真正要紧的地方 —— 建索引时**声明(`TYPE i64|f64|str|vector`)。某行的字段未通过声明的强转时,该行被排除出这个索引并计数(`IDX.VERIFY` / `IDX.LIST` 里的 `coerce_failures`)—— 一道声明式围栏,绝不是写入错误。

| RDS 类型 | kevy 形态 | 备注 |
|---|---|---|
| INT / BIGINT / SERIAL | 字段字节,索引 `TYPE i64` | 完整 i64 范围 |
| DECIMAL / NUMERIC | 以 `i64` 存**整数最小单位**(分、微)| kevy 没有 decimal 类型;`f64` 是二进制浮点 —— 永远别把钱放进去。`KIND agg` 的求和在 f64 里累积(精度界限已写明),所以选一个让总额远小于 2^53 的单位 |
| FLOAT / DOUBLE | 字段字节,索引 `TYPE f64` | IEEE 754 语义 |
| VARCHAR / CHAR / TEXT | 字段字节,索引 `TYPE str` | 二进制安全;按字节字典序 |
| BLOB / BYTEA | 字段字节(不建索引)或一个专用的字符串键 | 值在任何地方都是任意字节 |
| DATETIME / TIMESTAMP | Unix 纪元(秒或毫秒)作 `i64` | 范围索引给你 `BETWEEN`;kevy 没有日期算术 —— 日历运算归应用 |
| BOOL | `0` / `1` 作 `i64` | 可索引、可组合(`EQ 0`)|
| JSON / JSONB | 摊平成 hash 字段(`profile.city`)| 逐字段读取、TTL(`HEXPIRE`)、可索引;JSON-path 查询永久出局 —— cookbook 配方 9 |
| embedding(pgvector)| `dim × 4` 字节 f32-LE,索引 `TYPE vector` | [vector-search](../vector-search.md) |

## PRIMARY KEY、UNIQUE、AUTO_INCREMENT

**主键** —— 键本身。点查(`WHERE pk = ?`)就是 `GET`/`HGETALL`/`HMGET`:一个 shard、一次 hash 探测,不需要索引。

**AUTO_INCREMENT / 序列** —— `INCR seq:order` 铸一个 id;`INCRBY seq:order 100` 分配一个块,从应用内存里发号(高吞吐形态)。崩溃留空洞与 PostgreSQL 序列给你的契约相同(cookbook 配方 3)。

**UNIQUE** —— `KIND unique` 是**围栏,不是锁**:它不阻塞写(阻塞会把跨 shard 写串行化);重复项被计数(`IDX.VERIFY` 里的 `duplicates`),并在 `EQ` 读里以多命中形式可见。要一道硬性唯一闸门,用下面之一:

```
SET uniq:email:ada@example.com 42 NX          # atomic claim, NX = the gate
WATCH + MULTI/EXEC check-then-write           # CAS loop (cookbook recipe 4)
{hashtag} prefix + single-shard domain        # cluster mode
```

## SELECT

| SQL | kevy | verbs |
|---|---|---|
| `WHERE pk = ?` | 直接读键 | `GET` / `HGETALL` / `HMGET` |
| `WHERE col = ?` | 在声明的索引上做等值 | `IDX.QUERY idx EQ v [FIELDS f…]` |
| `WHERE col BETWEEN a AND b` | 在声明的索引上做范围扫描 | `IDX.QUERY idx RANGE a b` |
| `WHERE col > a` | 半开范围 | `RANGE a <max-sentinel>` |
| `WHERE a = ? AND b BETWEEN …` | 两索引组合 | `IDX.QUERY COMPOSE AND idxA EQ v idxB RANGE a b` |
| `WHERE col IN (v1, v2)` | 双腿 OR,或应用侧 N 次 EQ 查询 | `IDX.QUERY COMPOSE OR idx EQ v1 idx EQ v2` |
| `LIKE 'abc%'`(前缀)| str 范围索引,前缀边界 | `IDX.QUERY idx RANGE 'abc' 'abc\xff'` |
| `LIKE '%abc%'`(中缀)| **不支持这种形式** | 若形如 token 用全文 `MATCH`;否则重新设计字段 |
| 全文搜索 | `KIND text`,BM25 | `IDX.QUERY idx MATCH "…"` |
| `SELECT col1, col2`(投影)| 字段水合 | `… FIELDS col1 col2` |
| `COUNT(*) WHERE …` | 不物化的计数 | `IDX.COUNT idx RANGE a b` |
| `EXPLAIN` | 解析 + 计划行,零执行 | `IDX.EXPLAIN idx RANGE a b` |

语义与限制,如实说:

- **每索引一个字段。** 一个索引覆盖一个前缀的一个声明字段。多列谓词是恰好**两个**索引的 `COMPOSE AND|OR`(按键序 —— 两个值域不同,键序是唯一共享的顺序),或者用一个[视图](#view)承载最多 4 个叶子的具名可复用组合。更宽的临时合取就是滑向查询计划器的坡 —— 拒绝。剩余谓词在水合后于应用侧过滤。
- **`RANGE`/`EQ`/`COMPOSE`**:默认 `LIMIT 100`,上限 `10000`,游标分页(`[next-cursor, rows]`,`"0"` = 起点/耗尽)。游标契约是 SCAN 级的:在遍历期间保持稳定的行恰好被看到一次;并发写可能出现也可能不出现。没有全局快照 —— 与 `SCAN`/`DBSIZE` 同一信封。
- **前缀 `LIKE`**:`TYPE str KIND range` 索引按字节字典序有序,所以 `LIKE 'abc%'` 就是 `RANGE 'abc' 'abc<0xff>'` —— 一次索引扫描,不是键空间遍历。(用 `SCAN 0 MATCH 'user:*'` 匹配键名前缀也存在,但它增量式走完整个键空间 —— 用于运维杂务,绝不作服务路径。)
- **`MATCH`** 是对查询 token 的 OR 语义,BM25 排序,内置 CJK bigram;`LIMIT` 上限 1000,无游标;没有短语查询、没有布尔语法、没有高亮([text-search](../text-search.md))。
- **`FIELDS`** 在同一次调用里、在每行所属的 shard 上水合具名 hash 字段 —— 是"索引扫描 + 主键回查"双跳的单跳替代。它也是 JOIN 形状的水合原语(见下文)。
- **`IDX.EXPLAIN` 仅作诊断。** 它报告 kind、state、`est_rows` 和你已写好的查询的计划行 —— 没有在多个计划之间做选择的优化器。

## ORDER BY / LIMIT / OFFSET

- 一个 `range` 索引**就是**顺序:`IDX.QUERY … RANGE` 按索引值升序返回行。`ORDER BY col ASC LIMIT n` = 在 `col` 上声明一个范围索引,带 `LIMIT n` 查询。
- **降序**在裸索引面上:不是 `IDX.QUERY` 的选项。要么声明一个视图(`VIEW.CREATE … ORDER BY idx DESC`),要么在写入时存一个取负/取补的排序字段。
- **`ORDER BY a, b`**(复合):在写入时把复合编码进一个索引字段 —— 有界整数用 `a * 1_000_000 + b`,字典序复合用零填充字符串(cookbook 配方 8)。
- **`OFFSET` 不存在。** 有意不提供"跳过 N 行"—— 它在任何引擎里都是 O(N) 的浪费,深分页时是反模式。用返回的游标分页(每页常数成本,在 SCAN 级契约下位置稳定)。如果你的产品需要"跳到第 47 页",预计算页锚点或重新想 UX;kevy 不会替你藏住这笔成本。

## GROUP BY 与聚合

`KIND agg` 在**写路径里**维护聚合 —— 这是 `GROUP BY` 的倒置:RDS 在查询时扫行,kevy 在每次写落地时把它折进所属分组,读一个分组是 O(1)。

```
IDX.CREATE ord_amt ON PREFIX ord: FIELD amount TYPE i64 KIND agg GROUPBY status
IDX.QUERY ord_amt GROUP paid                  → [count, sum, min, max, avg]
IDX.QUERY ord_amt GROUPS BY sum LIMIT 100     → ranked [group, count, sum, min, max]
```

- `GROUP g` = `SELECT COUNT(*), SUM(v), MIN(v), MAX(v), AVG(v) WHERE group = g`;`GROUPS BY count|sum|min|max` = 排好序的 `GROUP BY … ORDER BY agg LIMIT n`(LIMIT ≤ 1000)。
- min/max 在删除之下保持精确(每分组一个值 multiset);求和在 f64 里累积 —— 涉及钱时注意精度界限(选一个让总额远小于 2^53 的最小单位)。
- **没有 `HAVING`、没有聚合表达式、没有对任意谓词的 `GROUP BY`** —— 在应用里过滤(有界的)`GROUPS` 结果。每个索引一个分组字段;要多种分组就声明多个 agg 索引。

## JOIN

**kevy 不做 join。** 没有任何 verb 在服务端把两个前缀连起来,以后也永远不会有(Law 3)。三种替代,按优先顺序:

1. **写入时反规范化。** 服务模型的答案:如果页面要在 `order.total` 旁边显示 `user.name`,就在订单行创建时把 `user_name` 写进去。存储便宜;写钩子让副本字段上的任何索引保持新鲜。父行更新的扇出(展示字段很少更新)交给一个 CDC 消费者。
2. **应用侧水合(两跳)。** 外键放在行里;一个索引让反向也只要一跳:`IDX.QUERY order_user EQ 42 FIELDS total status` 就是 `SELECT total, status FROM orders WHERE user_id = 42` —— 索引找到键并在同一次调用里水合字段(cookbook 配方 2)。正向就是按父行 `HMGET` —— 把 id 攒起来走 pipeline。
3. **带 `VIA` 水合的视图。** `VIEW.QUERY v FIELDS name VIA user:{key.1}` 通过模板把每个成员键解引用到一个*目标*键并在那里读字段 —— 一次声明的、可复用的成员→父行解引用(一次内部扇出,零客户端往返)。是解引用,不是查询:目标不接受谓词。

相比 SQL 你失去的:任意 N 路 join、join 时对远表的过滤、计划器选择的 join 顺序。你得到的:没有任何 join 会出现在延迟直方图里。

## VIEW

`VIEW.*` 是对声明索引的具名 AND/OR/DIFF 组合,外加一个排序索引 —— 更接近"带索引的*物化*视图",而不是 SQL 视图:

| SQL 视图性质 | kevy 视图 |
|---|---|
| 任意 SELECT 体 | **否** —— 只有索引形状的树(深度 ≤ 3、≤ 4 个叶子,形状在 CREATE 时固定)|
| 永远新鲜(虚拟)| `MODE virtual` —— 每次查询求值,零写入成本 |
| 物化 + REFRESH | `MODE materialized` —— 在写钩子里增量维护,**永不过期**,没有 refresh 任务;可选 `TOPK k` 界 |
| 视图内 ORDER BY | `ORDER BY <index> [DESC]` —— 一个声明的范围索引提供顺序 |
| 视图之上的视图 | 否 —— 只有一层,建在索引之上 |

```
VIEW.CREATE live_adults QUERY '( AND user_live EQ 0 user_age RANGE 18 200 )'
    ORDER BY user_age MODE materialized TOPK 100
VIEW.QUERY live_adults LIMIT 10 [FIELDS name] [VIA …]
```

杀手级应用是**热列表**:"按优先级排的前 100 个就绪任务"做成 `TOPK` 物化视图,写税约 2%,微秒级应答,永久新鲜 —— 这在 RDS 里需要覆盖索引 + 有纪律的查询 + 运气才能近似([views](../views.md))。

## 事务

kevy 的事务故事就是 Redis 的(Law 1),对到 SQL 上是这样:

| SQL | kevy | 差异 |
|---|---|---|
| `BEGIN … COMMIT`(批)| `MULTI` … `EXEC` | 命令排队并作为一个单元施加;**内部没有交互式读** —— 你要据以分支的读发生在 `MULTI` 之前 |
| `SELECT … FOR UPDATE` | `WATCH key` + `MULTI`/`EXEC` | 乐观 CAS,不是锁:被 WATCH 的键变了 `EXEC` 回 nil —— 重读重试(cookbook 配方 4)|
| `ROLLBACK` | 无 | 排队期错误中止整批(`-EXECABORT`,什么都没跑);`EXEC` 内的运行时错误**不会**撤销其他命令 —— Redis 语义 |
| 存储过程 / 一次原子 RMW | `EVAL`(Lua)| 整个脚本在 `KEYS[1]` 所属 shard 上是一个原子单元;可真正分支的读-判-写([lua](../lua.md))|
| 可串行化的单实体事务 | 嵌入式 `store.atomic(key, …)` | shard 加锁的闭包:读能看到自己的写,提交原子 + 一次 fsync(cookbook 配方 5)|
| 跨实体可串行化 | 嵌入式 `atomic_all_shards` | 确定的锁顺序;这是重锤 —— 慎用 |

隔离性,如实说:kevy 没有 MVCC,也没有隔离级别旋钮。在一个 shard 内,每条命令(以及每个 `EVAL`、`MULTI` 批或嵌入式 atomic 块)都被串行化 —— 凡是装得进一个 shard / 一个脚本的东西,你得到的就是*可串行化*行为。跨 shard 没有全局快照:多键读(`MGET`、`IDX.QUERY` 归并)是按 shard 原子、整体 SCAN 级。不存在观察到单条命令撕裂的读,也不存在对未提交 `MULTI` 批的脏读;但两条独立命令之间**没有**可重复读 —— 在乎的地方用 `WATCH`(CAS)或 Lua(单一单元)。

"提交"的耐久性:见 [persistence](persistence.md) —— `appendfsync always` 下,被确认的写在回复之前已落盘;默认 `everysec` 有最多 1 s 的窗口(Redis 的取舍)。`Store::fsync_aof()`(嵌入式)是按事务粒度的 `synchronous_commit` 逃生舱。

## 约束与触发器

| SQL | kevy 的答案 |
|---|---|
| `NOT NULL` / 类型检查 | **强转围栏**:未通过索引声明 TYPE 的行被排除并计数(`coerce_failures`);应用盯 `IDX.VERIFY` —— 声明式可见性,不是写入拒绝 |
| `CHECK (expr)` | 在原子单元内检验不变量:Lua 脚本(服务器)或 `atomic` 块(嵌入式)读、判、写 —— 引擎保证判定 + 提交是一个单元(cookbook 配方 5)|
| `UNIQUE` | 围栏 + 计数的重复项,或一道硬性 `SET … NX` 闸门(见上)|
| `FOREIGN KEY`(存在性)| 不强制;需要该不变量时,在一个原子单元内先写父后写子 |
| `ON DELETE CASCADE` | 应用模式:atomic 块(小规模)、`kevy-cli delete-prefix`(批量),或一个响应父行删除的 CDC 消费者(异步 —— cookbook 配方 10)|
| 触发器 | **CDC 消费者**:`FEED.READ` 把每次已提交的写作为变更帧投递 —— 提交之后、解耦、可重放,而且它不可能腐蚀写路径(cookbook 配方 10–12)|

有意不提供服务端约束或触发器 DSL:Lua 脚本是唯一的服务端逻辑,而且它*作为写入本身*运行,不是挂在写入上。

## 二级索引 DDL

| SQL | kevy |
|---|---|
| `CREATE INDEX idx ON t (col)` | `IDX.CREATE idx ON PREFIX t: FIELD col TYPE i64\|f64\|str KIND range` |
| `CREATE UNIQUE INDEX` | `KIND unique`(围栏语义,见上)|
| `CREATE INDEX … USING gin (tsvector)` | `KIND text`([text-search](../text-search.md))|
| pgvector `USING hnsw` | `KIND ann DIM d [DISTANCE cosine\|l2\|ip] [M m] [EF ef]`([vector-search](../vector-search.md))|
| `DROP INDEX` | `IDX.DROP idx` |
| `\d` / information_schema | `IDX.LIST` / `VIEW.LIST`(state、entries、bytes)|

- **在线构建**:`IDX.CREATE` 立即返回并在后台回填(每百万小行约 7 s;多 KB 文本体约 85 s/M)。就绪前查询回答 `-INDEXBUILDING` —— 轮询 `IDX.LIST` 等 `state=ready`([migration](../migration.md))。数据可用性从不等索引构建。
- 索引内容是**派生状态**:从不进快照、从不记 AOF,重启后在后台重建。目录(声明本身)持久化在数据目录的 sidecar 里。
- 最多 64 个索引;`MAXMEM` 以声明方式给构建设上限(`-INDEXOVERBUDGET`),而不是无界增长。
- **批量导入法则**:先导入,后声明索引 —— 以批量速度回填,胜过为每条导入行付写钩子的钱(cookbook 配方 15)。

## 分页与乐观锁模式

- **Keyset/游标分页**(好的那种 `OFFSET`):每个分页接口(`IDX.QUERY RANGE/EQ/COMPOSE`、`VIEW.QUERY`、`SCAN`)都返回 `[next-cursor, rows]`;把游标喂回去直到 `"0"`。把游标当作不透明、单次遍历用。
- **版本列乐观锁**:留一个 `version` 字段,`WATCH` 该行,读,`MULTI` + `HSET … version n+1` + `EXEC`;回 nil = 输了竞态,重试(cookbook 配方 4)。

## 备份与 PITR

| RDS | kevy |
|---|---|
| `mysqldump` / `pg_dump` | `kevy-cli export`(逻辑 RESP 流,兼容 `redis-cli --pipe`)|
| 二进制备份 | 快照文件(`SAVE`/`BGSAVE` → `dump-<id>.rdb`)|
| WAL / binlog | AOF(追加式命令日志,按 shard)|
| `synchronous_commit` | `appendfsync always`(或 `everysec` + `fsync_aof` 栅栏)|
| PITR(基线 + WAL 重放)| **恢复点契约**:快照 + 从快照记录的游标开始的 CDC 帧 = 之后任意游标处的精确状态([persistence](persistence.md))|

PITR 的范围说明:feed 窗口是内存里的 backlog(`feed_buffer_size`,上限 1 GiB/shard)—— 如果你依赖精确时点恢复,快照频率至少要跟上窗口的翻转。校验是一等公民:`PREFIX.DIGEST` / `kevy-cli diff` 能证明两个键空间相等,且对顺序与拓扑不敏感。

## 复制与读扩展

主节点 + N 个副本,默认异步 —— 就是读副本形态。与 RDS 的差别全在**一致性阶梯**([availability](availability.md))里,它按调用为每份保证定价,而不是按实例:

| RDS 概念 | kevy 阶梯级 |
|---|---|
| 异步副本(MySQL 默认)| 第 0 级:主节点本地施加后即 ack,副本尾随 |
| 会话级读己之写 | 第 1 级:主节点上 `REPL.TOKEN` → 副本上 `REPL.WAIT token` → 你的下一次读观察到你的写 |
| 半同步复制 | 第 2 级:每次写 `WAIT n timeout` —— 在主节点上**且**被 ≥ n 个副本确认(ACK ≠ fsync)|
| 副本最大滞后守卫 | 第 3 级:`replica_max_staleness_ms` —— 过期的副本回答 `-STALE`,而不是端出旧读 |
| 没有 standby 就拒绝写 | 第 4 级:`min_replicas_to_write`(`-NOREPLICAS`)|
| 围栏 / 脑裂守卫 | 第 5 级:多数派租约 —— 被分区的主节点围栏自己的写 |

切主:计划内交接是 `FAILOVER` verb(静默 → 追平 → 提升 —— 零丢失);崩溃切主是 3 节点 elect 多数派(MTTR ≈ 5–8 s,offset 最优的候选人胜出,因此 `WAIT` 确认过的写能幸存)。重新加入的前主节点的未复制尾巴会被**丢弃**(分叉后缀按定义从未被确认)—— 这与 MySQL 半同步里丢失未送出 binlog 的数据丢失形状相同,只是被写成了明文。每个降级状态都是一个有名字的错误(`-READONLY`、`-STALE`、`-NOREPLICAS`、`-QUIESCED`、`-MISDIRECTED`),路由客户端能据此自愈([error-replies](../error-replies.md))。

## CDC

feed 是 kevy 的"binlog 即 API"—— Debezium 从 RDS 里抽取的东西,这里原生供应([cdc](../cdc.md)):

| Debezium/binlog 概念 | kevy |
|---|---|
| binlog 位置 / LSN | `(generation, offset)` 游标,按 shard |
| connector 快照 + 流 | 重建(SCAN 你的前缀)+ 从 `FEED.TAIL` 续读 |
| 每表一个 topic 的过滤 | `FEED.READ` 上的 `PREFIX` 过滤(多键帧 fail-open)|
| at-least-once 投递 | 相同 —— 消费者必须幂等 |
| 事务性 outbox 模式 | **不需要**:每次已提交的写本来就是一个变更帧(cookbook 配方 11)|

有意不提供:全局跨 shard 顺序(shard 流内保证按键顺序)、服务端消费者位置、消费者组。留存就是内存 backlog;掉队会得到 `-FEEDRESYNC` 和用于重建的游标。

## kevy 不会做的事

拒绝清单,集中一处。每一条都是章程决定([designing-on-kevy](../designing-on-kevy.md),项目的 scope 日志)—— 不是路线图缺口:

| 拒绝 | 因为 | 改用 |
|---|---|---|
| SQL 解析器 / 查询 DSL | 计划器的滑坡从这里开始 | 显式 `IDX.*`/`VIEW.*` + 本矩阵 |
| 查询计划器 / 自动选索引 | 引擎必须执行声明的路径,而不是做决定 | `IDX.EXPLAIN`(仅诊断)|
| JOIN | 不做服务端计划搜索 | 反规范化 / 水合(`FIELDS`、`VIA`)/ 视图 |
| 没有索引的 `WHERE` | 意外的 O(n) 是一句等着半夜呼你的谎 | 声明路径,或别上线这个查询 |
| 服务端约束 DSL / 触发器 | 存储应用逻辑不是这个引擎的门类 | Lua `EVAL`(原子单元)+ CDC 消费者 |
| DECIMAL 类型 | i64/f64 之外没有服务端算术 | 整数最小单位 |
| JSON-path 查询 | hash 字段就是列模型 | 摊平成字段(cookbook 配方 9)|
| `HAVING` / 聚合表达式 | 查询语言的滑坡 | 在应用侧过滤 `GROUPS` 输出 |
| `OFFSET` 分页 | O(N) 跳行是反模式 | 游标 |
| 多数据库 `SELECT n` | 每台服务器一个键空间 | 前缀,或分开的实例 |
| AUTH / TLS | 信任边界是网络边界 | loopback 绑定(默认)、sidecar 代理 |
| 跨 DC 多活、CRDT | 单数据中心章程 | 应用层联邦 |
| 动态成员 / 自动顶替 / resharding | 拓扑由运维声明 | 配置 + 滚动重启 |
| HTTP/REST API | RESP + MCP 是仅有的两个访问平面 | 任意 Redis 客户端 |

## 容量估算与运维差异

**一切都常驻 RAM。** RDS 从磁盘换页的工作集,在这里活在内存;磁盘是耐久日志 + 快照,不是服务层。容量规划一行:

> RAM ≈ 行数 × (平均键长 + 平均行字节 + 每键开销)
> + Σ 各索引公式 + 视图成员数 × 条目大小 —— 然后在加载过的样本上
> 用 `MEMORY USAGE` / `IDX.LIST` 的 bytes 验证。

各子系统公式(每条都在 CI 里对实测 RSS 有 gate):范围索引 ≈ `rows × (value_width + avg_key_len + 48)`;文本与 ANN 公式见 [text-search](../text-search.md) / [vector-search](../vector-search.md)(1M × 1024 维向量 ≈ 4.1 GiB);agg ≈ 由分组数主导([indexes](../indexes.md));视图成员 ≈ `order_value_width + key_len + 48`([views](../views.md))。设 `maxmemory` + 一个驱逐策略,或接受 `-OOM` 拒绝(写在准入时被拒 —— 已有数据绝不腐蚀)。

**服务余量。** v3.17.0 发布竞技场(kevy vs valkey 9.1,公平对打协议,5 次取中位 —— `bench/PERF-LEDGER.md`):GET 3.00×、SET 4.00×、INCR/SADD 2.50×、HSET 2.25×、LPUSH 1.80×、ZADD 1.58× —— 完整复制/心跳流水线落地后 7/7 全胜。对上磁盘优先的 RDS 做点查,差距还要更大;那里诚实的比较是"kevy 同时替掉缓存层和业务查询",不是逐查询的 benchmark。

**与 RDS 的运维差异**,简述:单一键空间(没有 schema/database —— 前缀就是命名空间);TTL 是一等公民(`EXPIRE`、按字段的 `HEXPIRE` —— 定时删除的 cron 任务消失了);`FLUSHALL` 只有一条命令之遥(用边界保护它);派生状态(索引、视图、feed)重启后是重建而不是加载(每百万行数秒 —— 把它算进重启时间预算);错误面是契约的一部分 —— 客户端应匹配错误前缀,而不是解析消息([error-replies](../error-replies.md))。

## 本页 vs cookbook

本页是**参考矩阵** —— SQL 构造进,kevy 形态出,差异写明。[cookbook](../cookbook.md) 是**配方书** —— 18 个可运行的模式(每个命令块都过 CI 冒烟),每个都标注它替代的 SQL 构造,并交叉链接回上面的矩阵行。真要迁移?分阶段的 playbook 在 [migration](../migration.md)。
