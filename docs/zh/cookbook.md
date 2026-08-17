# RDS→kevy 建模 cookbook

你正在把一个关系型数据模型搬上 kevy。下面每个 recipe 用的都是已交付的原语——没有 roadmap 功能，没有「即将推出」。每一条都点名它替代的 RDS 概念，和承接它的 kevy 模式。

所有 recipe 背后的设计立场是：**建模访问路径，而不是 schema**。RDS 允许你把这个决定推迟给查询规划器；kevy 要你把它说出来——回报是服务时的微秒级页面（实测数字见 `bench/VALIDATION-LEDGER.md`）。

每个命令块都能对一台新起的本地 kevy 原样运行（`kevy --port 6004`；recipe 11–14、16 和 20 还需要 `kevy.toml` 里 `[feed] enabled = true`——见 [docs/cdc.md](cdc.md)）。`bench/cookbook_smoke.sh` 会把（英文版）cookbook 里的每一行 `kevy-cli` 对一台一次性服务器执行一遍，保证这些命令块永远诚实。

## 1. 表与行

**SQL 对应物**：`CREATE TABLE` + `SELECT col FROM t WHERE id = ?`——[矩阵：表、行、列](rds-workloads.md#表行列)。

一行就是一个带类型前缀的 hash：

```console
kevy-cli -p 6004 HSET user:42 name ada email ada@example.com age 36
kevy-cli -p 6004 HGET user:42 name
kevy-cli -p 6004 HGET user:42 phone    # NULL = 字段缺席:天然回答 (nil)
```

- 表 → key 前缀（`user:`）。列 → hash 字段。主键 → key 本身。
- **NULL = 字段缺席。**不要存哨兵字符串；`HGET` 缺失字段天然回答 nil，索引规格把缺失字段视为「该行被排除」（在 `IDX.VERIFY` 计数里可见）。
- 列类型由你定：kevy 存的是字节。在类型真正要紧的地方声明它——创建索引时（`TYPE i64|f64|str|vector`）；强转失败会被计数，绝不静默入索引。

## 2. 一对多、多对多

**SQL 对应物**：外键列 + 关联表；`SELECT … FROM orders WHERE user_id = ?`——[矩阵：JOIN](rds-workloads.md#join)。

关系由链接 key 承载，每一侧一个 set：

```console
kevy-cli -p 6004 HSET order:1001 user_id 42 total 1999 status shipped
kevy-cli -p 6004 HSET order:1002 user_id 42 total 550 status pending
kevy-cli -p 6004 SADD user:42:orders 1001 1002       # 1-N:成员 = 订单 id
kevy-cli -p 6004 RPUSH order:1001:items sku-7 sku-9
kevy-cli -p 6004 SADD tag:urgent:orders 1001         # N-M:每侧一个 set
kevy-cli -p 6004 SADD order:1001:tags urgent
```

或者干脆跳过链接 key：把外键放进行里（上面的 `user_id`），声明一个索引——`IDX.QUERY … EQ 42` 就是这个世界的 `SELECT … WHERE user_id = 42`，一跳完成 hydrate：

```console
kevy-cli -p 6004 IDX.CREATE order_user ON PREFIX order: FIELD user_id TYPE i64 KIND range
kevy-cli -p 6004 IDX.QUERY order_user EQ 42 FIELDS total status
```

## 3. 序列

**SQL 对应物**：`AUTO_INCREMENT` / `CREATE SEQUENCE` + `nextval()`——[矩阵：PRIMARY KEY、UNIQUE、AUTO_INCREMENT](rds-workloads.md#primary-keyuniqueauto_increment)。

```console
kevy-cli -p 6004 INCR seq:order          # 取一个 id
kevy-cli -p 6004 INCRBY seq:order 100    # 块分配:一次发 100 个 id
                                         # 在应用内存里派发,用完再取
```

块分配是高吞吐形态；崩溃留缝的契约与 PostgreSQL 序列一致。

## 4. 乐观锁（行版本）

**SQL 对应物**：`UPDATE t SET …, version = v+1 WHERE id = ? AND version = v`（版本列 CAS）——[矩阵：事务](rds-workloads.md#事务)。

服务器侧：WATCH/MULTI——CAS 循环。事务是连接作用域的，所以要在一个 REPL 会话里跑（这里用 heredoc 喂入）：

```bash
kevy-cli -p 6004 HSET user:42 balance 100 version 7
kevy-cli -p 6004 <<'TXN'
WATCH user:42
HGET user:42 version
MULTI
HSET user:42 balance 90 version 8
EXEC
TXN
```

`WATCH` 之后有人动过 `user:42` 的话，`EXEC` 回答 nil——竞争输了；重读重试。

Embedded 侧：把「读—判断—写」放进一个 `atomic()` 块——shard 锁让这个分支天然无竞争，不需要重试循环。

## 5. CHECK 约束与多 key 不变量

**SQL 对应物**：`CHECK (balance >= 0)` + 触发器维护的审计行——[矩阵：约束与触发器](rds-workloads.md#约束与触发器)。

RDS 在引擎里跑 `CHECK (balance >= 0)`。kevy 的替代是**原子块内的读**：应用评估不变量，引擎保证判断与写提交在一起。

```rust
// embedded——不许透支的扣款,附带一条审计行:
store.atomic(b"acct:7", |ctx| {
    let bal: i64 = parse(ctx.hget(b"acct:7", b"balance")?);
    if bal < amount { return Err(Overdraw); }
    ctx.hset(b"acct:7", &[(b"balance", &(bal - amount))])?;
    ctx.rpush(b"acct:7:ledger", &[entry])?;
    Ok(())
})
```

跨 shard 不变量：`atomic_all_shards`（确定性锁序，文档化的死锁豁免）。慎用——它是可串行化事务的大锤，而多数不变量按设计就住在一个 key 前缀底下。

## 6. 幂等键

**SQL 对应物**：`UNIQUE INDEX` + `INSERT … ON CONFLICT DO NOTHING`——[矩阵：PRIMARY KEY、UNIQUE、AUTO_INCREMENT](rds-workloads.md#primary-keyuniqueauto_increment)。

```console
kevy-cli -p 6004 HSET req:9001 idem_key pay-2026-07-04-a77 amount 1999
kevy-cli -p 6004 IDX.CREATE req_idem ON PREFIX req: FIELD idem_key TYPE str KIND unique
kevy-cli -p 6004 IDX.QUERY req_idem EQ pay-2026-07-04-a77   # 重复以多命中读的形式可见
kevy-cli -p 6004 IDX.VERIFY req_idem                        # ……并在这里被计数
kevy-cli -p 6004 SET idem:pay-2026-07-04-a77 1 NX PX 86400000
```

先写行、再查询——重复是*可见的*（unique kind 在 VERIFY 里计数而不是拒绝写入；这是声明式栅栏，不是写闸门）。要硬闸门，就在处理之前用 `SET … NX PX` 形态：NX 是原子认领，TTL 是保留窗口。

## 7. 软删除

**SQL 对应物**：`deleted` 标记列 + 部分索引 / 视图 `WHERE deleted = 0`——[矩阵：VIEW](rds-workloads.md#view)。

打标记，不移除：

```console
kevy-cli -p 6004 HSET user:42 deleted 0 age 36
kevy-cli -p 6004 HSET user:43 deleted 1 age 51
kevy-cli -p 6004 IDX.CREATE user_live ON PREFIX user: FIELD deleted TYPE i64 KIND range
kevy-cli -p 6004 IDX.QUERY user_live EQ 0 LIMIT 100    # 只要活的行
```

视图把过滤条件永久组合掉——调用方再也不用复述它：

```console
kevy-cli -p 6004 IDX.CREATE user_age ON PREFIX user: FIELD age TYPE i64 KIND range
kevy-cli -p 6004 VIEW.CREATE live_users QUERY '(' AND user_live EQ 0 user_age RANGE 18 200 ')' ORDER BY user_age
kevy-cli -p 6004 VIEW.QUERY live_users LIMIT 10
```

## 8. 复合排序（ORDER BY a, b）

**SQL 对应物**：复合索引上的 `ORDER BY a, b`——[矩阵：ORDER BY / LIMIT / OFFSET](rds-workloads.md#order-by--limit--offset)。

写入时把复合序编码进一个被索引的 score 字段：有界整数 `b` 用 `score = a * 1_000_000 + b`，字典序复合用零填充的字符串字段——一个索引、一个 ORDER BY；写钩子像维护普通字段一样维护它：

```console
kevy-cli -p 6004 HSET evt:1 ord '2026-07-04|000042'
kevy-cli -p 6004 HSET evt:2 ord '2026-07-04|000007'
kevy-cli -p 6004 HSET evt:3 ord '2026-07-05|000001'
kevy-cli -p 6004 IDX.CREATE evt_ord ON PREFIX evt: FIELD ord TYPE str KIND range
kevy-cli -p 6004 IDX.QUERY evt_ord RANGE '2026-07-04|000000' '2026-07-04|999999' LIMIT 100
```

## 9. JSONB

**SQL 对应物**：JSON/JSONB 列 + 生成列索引——[矩阵：类型系统](rds-workloads.md#类型系统)。

拍平成 hash 字段：`profile.city` → 字段 `profile.city`。你保住了按字段读写、字段级 TTL（HEXPIRE）和可索引性——JSONB 给你的一切，除了 JSON-path 查询，那是**永久出局**的（查询引擎斜坡；见 [designing-on-kevy.md](designing-on-kevy.md) 的 REFUSED 表）。

```console
kevy-cli -p 6004 HSET user:7 profile.city tokyo profile.plan pro
kevy-cli -p 6004 HGET user:7 profile.city
kevy-cli -p 6004 HEXPIRE user:7 3600 FIELDS 1 profile.plan   # 字段级 TTL 在拍平后依然可用
```

没人索引的深嵌套 blob 可以留成一个序列化字段；某条路径一旦要紧，就把它提升成字段。

## 10. 级联删除 / 外键

**SQL 对应物**：`FOREIGN KEY … ON DELETE CASCADE`——[矩阵：约束与触发器](rds-workloads.md#约束与触发器)。

级联是应用模式，从来不是引擎魔法：

- 同步、小爆炸半径：在一个原子块内删除（`ctx.del(row)`、`ctx.srem(parent_link, id)`）。
- 批量 / 前缀形状：`delete-prefix`——限速、可续传。
- 异步：CDC 消费者（带 `PREFIX` 的 `FEED.READ`）响应父行删除并清理子行——触发器的替代品，提交后、解耦、可重放。

```console
kevy-cli -p 6004 HSET order:1001 user_id 42
kevy-cli -p 6004 RPUSH order:1001:items sku-7 sku-9
kevy-cli -p 6004 SADD order:1001:tags urgent
kevy-cli delete-prefix -p 6004 --rate 5000 order:1001:   # 子行清空,父行还在
```

## 11. 你不需要的 outbox

**SQL 对应物**：事务性 outbox 表 + 中继 worker——[矩阵：CDC](rds-workloads.md#cdc)。

事务性 outbox 模式之所以存在，是因为 RDS 提交和消息总线发布无法原子化。在 kevy 里**feed 就是 outbox**：每笔已提交的写入本来就是一个位于 `(generation, offset)` 游标处的变更帧，at-least-once、可按前缀过滤（[docs/cdc.md](cdc.md)）。消费 `FEED.READ`；别再造第二本日志。

```console
# 需要 kevy.toml 里 [feed] enabled = true(见 ../cdc.md)
kevy-cli -p 6004 HSET order:9001 status paid
kevy-cli -p 6004 FEED.SHARDS
kevy-cli -p 6004 FEED.TAIL 0                             # 新消费者的起始游标
kevy-cli -p 6004 FEED.READ 0 $(kevy-cli -p 6004 FEED.TAIL 0 | head -1 | awk '{print $3}') 0 COUNT 10 PREFIX order:  # generation 从 FEED.TAIL 读:它是身份,不是计数器
```

## 12. 审计历史

**SQL 对应物**：触发器维护的审计/历史表（或 binlog 考古）——[矩阵：CDC](rds-workloads.md#cdc)。

CDC 的保留窗口就是审计日志：帧按提交顺序携带已应用效果的 argv。按你欠合规的窗口给 feed backlog 定容量，用游标消费者导出到冷存储。要做时间点重建：恢复快照 + 重放到 `(gen, offset)` 恢复点（[persistence.md](persistence.md)）。

```console
kevy-cli -p 6004 HSET acct:7 balance 100
kevy-cli -p 6004 HSET acct:7 balance 90
kevy-cli -p 6004 FEED.READ 0 $(kevy-cli -p 6004 FEED.TAIL 0 | head -1 | awk '{print $3}') 0 COUNT 100 PREFIX acct:  # 谁在什么时候写了什么,按提交序
```

## 13. 回滚窗口（反向镜像）

**SQL 对应物**：切换期间反向复制回旧主库——[迁移 playbook 阶段 5](migration.md)。

切换期间，跑一个把 kevy 写入镜像回旧 RDS 的 CDC 消费者（`FEED.READ` → UPDATE 语句）。这样你的回滚方案是「把应用指回去」，不是「反向迁移数据」。信心固化后退役镜像；`kevy-cli diff`（按前缀摘要）就是信心仪表。

```console
kevy-cli -p 6004 HSET user:42 name ada
kevy-cli -p 6004 FEED.READ 0 $(kevy-cli -p 6004 FEED.TAIL 0 | head -1 | awk '{print $3}') 0 COUNT 10 PREFIX user:  # 镜像消费者的读循环
kevy-cli diff 127.0.0.1:6004 127.0.0.1:6004 user:        # 摘要一致:安全形态的自检
kevy-cli diff old-rds-mirror.internal:6379 127.0.0.1:6004 user:   # needs-external
```

## 14. 分析导出

**SQL 对应物**：喂数仓的 ETL 作业 / binlog tap——[矩阵：CDC](rds-workloads.md#cdc)。

服务与分析不共用一个引擎。导出模式：

- `export`——逻辑导出、可续传、RESP 走到哪就能载到哪。
- CDC → 数仓：游标消费者把插入流进你的 OLAP 存储，正是 CDC-to-Kafka 的形状。
- 只读 listener（[embedded-listener.md](embedded-listener.md)）供 embedded 应用做临时抽取。

```console
kevy-cli -p 6004 HSET order:1001 user_id 42 total 1999
kevy-cli export -p 6004 --prefix order: /tmp/orders.resp
kevy-cli -p 6004 FEED.READ 0 $(kevy-cli -p 6004 FEED.TAIL 0 | head -1 | awk '{print $3}') 0 COUNT 100 PREFIX order:  # CDC 到数仓的读循环
```

## 15. 载入顺序（延迟索引规则）

**SQL 对应物**：先 `LOAD DATA`、后 `CREATE INDEX`（批量载入纪律）——[矩阵：二级索引 DDL](rds-workloads.md#二级索引-ddl)。

**先**批量载入，**后**声明索引/视图：回填以约 7s/百万行的速度从既有行构建——比给每条导入行付写钩子便宜几个数量级（[migration.md](migration.md)）。

```console
kevy-cli -p 6004 HSET item:1 price 10
kevy-cli -p 6004 HSET item:2 price 25
kevy-cli -p 6004 HSET item:3 price 7
kevy-cli export -p 6004 --prefix item: /tmp/items.resp
kevy-cli import -p 6004 /tmp/items.resp   # 先批量载入:不付索引写钩子
kevy-cli -p 6004 IDX.CREATE item_price ON PREFIX item: FIELD price TYPE i64 KIND range   # 后声明:走回填
kevy-cli -p 6004 IDX.QUERY item_price RANGE 0 100 LIMIT 10
```

---

后面三个 recipe 换了负载：不再是被替换的 RDS，而是 AI agent 的记忆栈。不需要任何新东西——会话状态、情景记忆和 RAG 检索，就是同一批访问路径模式换了 key 前缀。

## 16. 带 TTL 的会话上下文

**SQL 对应物**：sessions 表 + 过期 cron——[矩阵：容量估算与运维差异](rds-workloads.md#容量估算与运维差异)。

agent 的工作上下文是一行带租约的数据：压缩后的对话住在 hash 里，`EXPIRE` 是闲置逐出策略（每轮续租——滑动窗口），feed 是审计轨迹——有人问「agent 在第 7 轮知道什么」时拿来重放。

```console
# 需要 kevy.toml 里 [feed] enabled = true(见 ../cdc.md)
kevy-cli -p 6004 HSET session:a7 user 42 turns 6 messages 'wants refund for order 1001; tone calm' last_tool order_lookup
kevy-cli -p 6004 EXPIRE session:a7 3600
kevy-cli -p 6004 HSET session:a7 turns 7 messages 'refund approved; awaiting confirmation'
kevy-cli -p 6004 EXPIRE session:a7 3600                       # 每一轮都续租
kevy-cli -p 6004 FEED.TAIL 0                                  # 审计游标:日志现在的尾部
kevy-cli -p 6004 FEED.READ 0 $(kevy-cli -p 6004 FEED.TAIL 0 | head -1 | awk '{print $3}') 0 COUNT 100 PREFIX session:  # generation 从 FEED.TAIL 读:它是身份,不是计数器
```

`messages` 字段装的是你的压缩步骤产出的任何摘要；重写它就是一条 `HSET`，而每次修订本来就是提交序里的一个变更帧——多数 agent 框架外挂的「对话历史」表，就是 recipe 12 的审计日志白送给你。

## 17. 情景记忆（时间 × 语义）

**SQL 对应物**：`WHERE ts BETWEEN …` + pgvector `ORDER BY embedding <=> ? LIMIT k`——[矩阵：SELECT](rds-workloads.md#select)。

情景记忆对同一批行回答两个问题：*最近发生了什么*（时间）和*什么与此相似*（语义）。一个前缀，每个问题一个索引——`DIM 8` 是为了演示可读；真实 embedding 是 768+ 维、以 f32-LE blob 传输，下面的 `csv:` 调试形态在任何接受向量的地方都可用（存储字段与查询向量走同一个解析器——[vector-search.md](vector-search.md)）。

```console
kevy-cli -p 6004 HSET mem:1 ts 1783200000 kind obs what 'user prefers dark roast' v csv:0.9,0.1,0,0,0,0,0,0
kevy-cli -p 6004 HSET mem:2 ts 1783203600 kind obs what 'user asked about decaf' v csv:0.8,0.3,0.1,0,0,0,0,0
kevy-cli -p 6004 HSET mem:3 ts 1783207200 kind reflection what 'coffee questions cluster in the morning' v csv:0,0.2,0.9,0.1,0,0,0,0
kevy-cli -p 6004 IDX.CREATE mem_ts ON PREFIX mem: FIELD ts TYPE i64 KIND range
kevy-cli -p 6004 IDX.CREATE mem_kind ON PREFIX mem: FIELD kind TYPE str KIND range
kevy-cli -p 6004 IDX.CREATE mem_ann ON PREFIX mem: FIELD v TYPE vector KIND ann DIM 8
kevy-cli -p 6004 IDX.QUERY mem_ts RANGE 1783203000 1783210000 LIMIT 10 FIELDS what      # 最近的记忆
kevy-cli -p 6004 IDX.QUERY mem_ann KNN csv:0.85,0.2,0,0,0,0,0,0 LIMIT 2 FIELDS what ts  # 相似的记忆
kevy-cli -p 6004 IDX.QUERY COMPOSE AND mem_ts RANGE 1783203000 1783210000 mem_kind EQ reflection LIMIT 10 FIELDS what
```

`COMPOSE AND` 合取标量腿（`RANGE`/`EQ`）——这里是「在这个时间窗内 AND 是一条 reflection」。至于*窗口内的相似*，有意不提供 KNN 腿（在图遍历里做过滤是查询引擎斜坡，REFUSED）：给 `LIMIT` 留余量跑 KNN，像上面那样用 `FIELDS` hydrate `ts`，窗口外的命中在客户端丢弃。

## 18. 带混合检索的 RAG 分块

**SQL 对应物**：tsvector 全文 + pgvector KNN，应用侧融合——[矩阵：SELECT](rds-workloads.md#select)。

chunk 是同时携带两个检索面的行——文本和它的 embedding——一次写同时维护两个索引：

```console
kevy-cli -p 6004 HSET chunk:1 doc kevy-guide seq 1 body 'rows are hashes under a typed key prefix' v csv:1,0,0,0,0,0,0,0
kevy-cli -p 6004 HSET chunk:2 doc kevy-guide seq 2 body 'indexes are declared once and maintained by the write hook' v csv:0,1,0,0,0,0,0,0
kevy-cli -p 6004 HSET chunk:3 doc kevy-guide seq 3 body 'the feed streams every committed write as a change frame' v csv:0,0,1,0,0,0,0,0
kevy-cli -p 6004 IDX.CREATE chunk_text ON PREFIX chunk: FIELD body TYPE str KIND text
kevy-cli -p 6004 IDX.CREATE chunk_ann ON PREFIX chunk: FIELD v TYPE vector KIND ann DIM 8
kevy-cli -p 6004 IDX.QUERY HYBRID chunk_text MATCH 'typed key prefix' chunk_ann KNN csv:0.9,0.1,0.1,0,0,0,0,0 LIMIT 2 FIELDS body
kevy-cli -p 6004 IDX.QUERY HYBRID chunk_text MATCH 'change frame' chunk_ann KNN csv:0,0.1,0.9,0,0,0,0,0 LIMIT 2 RRFK 20 FIELDS body
```

`HYBRID` 在服务端跑两条腿并以**倒数排名融合**（RRF）合并：每个 key 在 BM25 列表与 KNN 列表上各取 `Σ 1/(k + rank)`——只看排名，所以两种异质分数尺度永远不需要归一化；同时在*两条*腿都靠前的 chunk，胜过只霸一条腿的 chunk。`RRFK` 就是那个 k（默认 60）：信得过每条腿的头部命中、想让「两边都同意」占主导时调低；想把融合拉平、让共识深入两个列表时调高。

---

最后两个 recipe 彻底离开机架：边缘节点上的 kevy——同一个服务器二进制，或裁到 `core` 档 655 KB 的 `kevy-embedded`（[iot.md](iot.md)）——说的还是同一套 verb，模式从数据中心到传感器网关逐字迁移。

## 19. 传感器缓存（最新值 + 存活租约）

**SQL 对应物**：`readings_latest` upsert 表 + 陈旧度 cron——[矩阵：容量估算与运维差异](rds-workloads.md#容量估算与运维差异)。

每个传感器的当前值是一行数据；TTL 是存活契约。停止上报的传感器会从缓存中过期消失——**缺席本身就是掉线信号**，不用写清扫任务：

```console
kevy-cli -p 6004 HSET sensor:t1 val 21.5 unit C ts 1783200000
kevy-cli -p 6004 EXPIRE sensor:t1 90
kevy-cli -p 6004 HSET sensor:t1 val 21.7 unit C ts 1783200030
kevy-cli -p 6004 EXPIRE sensor:t1 90      # 每次上报都续租
kevy-cli -p 6004 EXISTS sensor:t1         # 1 = 在报,0 = 已失联
```

租约按你的告警容忍度定（这里 90 s = 连丢三个 30 秒上报）。要*响应*传感器失联而不是轮询，就开启带 `x`（expired）类的 keyspace 通知并订阅过期事件——同一契约的推送形态（[pubsub.md](pubsub.md)）。

最近窗口是一条带硬上限的 stream——`MAXLEN ~` 让节点内存无论跑多久都有界，这在动辄数月不重启的边缘盒子上正是要紧的不变量：

```console
kevy-cli -p 6004 XADD sensor:t1:log MAXLEN '~' 1000 '*' val 21.5
kevy-cli -p 6004 XADD sensor:t1:log MAXLEN '~' 1000 '*' val 21.7
kevy-cli -p 6004 XLEN sensor:t1:log
kevy-cli -p 6004 XRANGE sensor:t1:log - + COUNT 10
```

Embedded 形态：同一批 verb 走网关进程内的类型化 API——`store.hset(…)` / `store.expire(…)` / `store.xadd(…)`——完全不经过 socket；`core` feature 档承载本 recipe 用到的一切（[iot.md](iot.md)）。

## 20. 边缘聚合（写时 GROUP BY + 上行）

**SQL 对应物**：每次仪表盘刷新重跑一遍的 `SELECT zone, COUNT(*), SUM(w) … GROUP BY zone`——[矩阵：GROUP BY 与聚合](rds-workloads.md#group-by-与聚合)。

边缘节点就地汇总、只上传摘要——原始读数多到没法上行。聚合声明一次，由写路径维护，于是「聚合作业」直接不复存在：

```console
kevy-cli -p 6004 HSET reading:1 zone floor1 w 120
kevy-cli -p 6004 HSET reading:2 zone floor1 w 180
kevy-cli -p 6004 HSET reading:3 zone floor2 w 95
kevy-cli -p 6004 IDX.CREATE zone_w ON PREFIX reading: FIELD w TYPE i64 KIND agg GROUPBY zone
kevy-cli -p 6004 IDX.QUERY zone_w GROUP floor1            # [count, sum, min, max, avg]
kevy-cli -p 6004 IDX.QUERY zone_w GROUPS BY sum LIMIT 10  # 按负载排名的 zone
```

上行就是 recipe 11 的 outbox 穿上工装：feed 本来就按提交顺序记着每笔写入，所以云同步消费者就是一个游标循环，链路断几个小时也能续——at-least-once、按提交序、按前缀过滤出云端要的那部分：

```console
# 需要 kevy.toml 里 [feed] enabled = true(见 ../cdc.md)
kevy-cli -p 6004 FEED.TAIL 0
kevy-cli -p 6004 FEED.READ 0 $(kevy-cli -p 6004 FEED.TAIL 0 | head -1 | awk '{print $3}') 0 COUNT 100 PREFIX reading:  # 上行循环
```

再配上 recipe 19 的 `MAXLEN` 上限和 TTL：原始读数在节点上有界，聚合行始终很小，feed 游标跨重启存活——除了 kevy 本身，整个边缘故事零活动件。

## 21. 派生状态作为行的纯函数

**SQL 对应：**整个触发器层，一次全包——`ON DELETE CASCADE`、`UNIQUE` 约束，以及你在不再信任它们之后写的那个对账任务——[矩阵：约束与触发器](rds-workloads.md#约束与触发器)。

这是上面那些配方一直在绕的模式：§2 的链接键、§5 的不变量、§10 的级联、§12 的审计行，是同一个想法用了四遍。**把它说清楚一次，级联、唯一性与漂移检测就一起解决了。** 它来自一次真实迁移，那次花了一天才走到这里——这一天值得替你省下。

**想法：**写一个从行到"由它派生出的每一个键"的**纯函数**。不是一个去更新键的过程——是一个**返回**"应该存在什么"的函数。

```rust
// Everything user:42 implies, computed from the row alone.
fn derived(id: &[u8], row: &Row) -> Vec<Vec<u8>> {
    vec![
        key(b"email:", &row.email),          // uniqueness claim
        key(b"dept:", &row.dept, b":users"), // membership
    ]
}
```

于是每个操作都成了一次 diff，而且每一条都是**落出来的**，不是设计出来的：

| 操作 | 你做什么 | 白得什么 |
|---|---|---|
| **插入** | 加上 `derived(new)` | 声明与成员关系一起出现 |
| **更新** | 加上 `derived(new) - derived(old)`，删掉 `derived(old) - derived(new)` | 改过的 email **会释放它旧的声明**——那正是人人手写时都会写出的 bug |
| **删除** | 删掉 `derived(old)` | 级联不再是一条单独的代码路径 |
| **校验** | 对每一行重算 `derived`，与实际存在的对账 | 一个你没设计过的漂移检测器 |

更新那一行，是这个模式的回本处。手写的级联代码几乎总是加上新声明、忘掉释放旧的——因为"释放"是**没人会在工单里演示**的那种情形。

```rust
store.atomic_all_shards(|ctx| {
    let old = read_row(ctx, id)?;
    let (want, had) = (derived(id, &new), derived(id, &old));

    for k in want.iter().filter(|k| !had.contains(k)) {
        if ctx.exists(&[k]) > 0 { return Err(Taken); }  // uniqueness
        ctx.set(k, id);
    }
    for k in had.iter().filter(|k| !want.contains(k)) {
        ctx.del(&[k]);                                  // release
    }
    write_row(ctx, id, &new)
})
```

返回 `Err` 会把整件事回滚（§5），所以一次被拒的写入，既不会留下行，也不会留下半套已生效的声明。

**是声明，还是索引？**一个唯一性声明是**第二个真相来源**，它可能与行漂移；而[二级索引](indexes.md)是**按构造派生**的，不会。在 `atomic_all_shards` 里你可以直接查它：

```rust
if ctx.idx_count(b"email_idx", &want, &want)? > 0 { return Err(Taken); }
```

两条限制，都是刻意的：

- **只在 `atomic_all_shards` 上。** 一条索引条目住在它所索引的那个键的分片上，所以"有没有任何行用了这个 email"是一个关于**每一个**分片的问题。单分片的 `atomic()` 只持有一把锁，只能替它自己那一片作答——一个只查了 1/N 键空间的唯一性检查，几乎总会报"唯一"，所以**它不被提供**，而不是提供了再加个脚注。
- **索引读看不见本事务自己的写。** 维护发生在提交时。一个在同一闭包里插入两行的写者，必须自己把它们互相比一遍。

**校验。**因为 `derived` 是个函数，**检查器就是那个函数**——把同一个传给 `reconcile`：

```rust
let report = store.snapshot().reconcile(
    b"user:",                    // the rows
    &[b"email:", b"dept:"],      // where derived keys live
    |key, row| derived(key, row),
);
if !report.is_clean() {
    warn!("{} missing, {} orphaned", report.missing_count, report.orphaned_count);
}
```

它**双向**对账，而这正是值得不自己写的那一半。**缺失的键**是丢掉的派生状态；而**孤儿**——行已经没了、声明还在——才是一次半生效的更新会留下的东西，也是那个**会静默挡住后续插入**的故障。一个只找缺失的检查器，恰恰会在这种故障发生时报"干净"。

它跑在快照上（`store.snapshot()`），在每一把分片锁下冻结，所以它不会把一次并发写误当成漂移。**但这只在写本身是原子的前提下成立**：一行和它的声明必须进同一个 `atomic_all_shards` 块，否则真的存在一个半生效状态等着被找到。对账与原子写，是同一个保证的两端。

开机时跑，或者定时跑，或者在你完全信任写路径之后就不跑——但**要跑**，因为它是唯一能告诉你"你相信的那个不变量就是你实际拥有的那个"的东西。

## 22. 移植一份 PG/MySQL schema

**SQL 对应：**schema 文件本身——`CREATE TABLE`、`CREATE INDEX`、`CREATE VIEW`——[矩阵：二级索引 DDL](rds-workloads.md#二级索引-ddl)。

配方 1–8 手工做的一切，从你**已经有**的那份 SQL 编译出来。`kevy-sql`（以及它的 `kevy-cli sql` 那张面孔）是一个**声明期编译器**：它像迁移工具一样把 schema **读一次**，产出显式的 `TABLE.DECLARE` / `VIEW.CREATE` 命令，外加*查询卡片*——带 `$N` 槽位的现成 `IDX.QUERY` 模板。服务器里**没有任何东西按查询运行**；运行期的 ad-hoc SQL 仍由引擎自己拒绝（Law 3）。

这份 schema——[docs/examples/shop.sql](https://github.com/goliajp/kevy/blob/develop/docs/examples/shop.sql)，一份真实的 users/orders/order_items 精简版：

```sql
CREATE TABLE users (
  id     bigserial PRIMARY KEY,
  email  text,
  name   text,
  plan   text
);
CREATE UNIQUE INDEX ON users (email);

CREATE TABLE orders (
  id          bigserial PRIMARY KEY,
  user_id     bigint,
  status      text,
  total       numeric(10,2),
  created_at  bigint       -- epoch seconds, app-encoded
);
-- INCLUDE = PG covering columns -> kevy stored VALUES (residual FILTER/SORT).
CREATE INDEX ON orders (status) INCLUDE (total, created_at);
-- Multi-column -> a composite ORDERPATH (the (user_id, created_at DESC) walk).
CREATE INDEX ON orders (user_id, created_at DESC);

CREATE TABLE order_items (
  id        bigserial PRIMARY KEY,
  order_id  bigint,
  sku       text,
  qty       int
);
CREATE INDEX ON order_items (order_id);

CREATE VIEW paid_orders AS
  SELECT * FROM orders WHERE status = 'paid';

CREATE VIEW recent_orders_by_user AS
  SELECT id, status, total, created_at FROM orders
  WHERE user_id = $1
  ORDER BY created_at DESC
  LIMIT 20;
```

编译它，然后把声明应用到一台服务器上：

```console
kevy-cli sql compile docs/examples/shop.sql
kevy-cli sql compile docs/examples/shop.sql --apply --url 127.0.0.1:6004
```

编译出来的脚本（原样）。每张表把它的索引折进**一条** `TABLE.DECLARE`；常量视图变成引擎视图；带参数的视图变成查询卡片；每一处粗粒度的类型映射都在 notes 里被**如实点名**（kevy 的列只有 `i64|f64|str`——时间戳由应用编码，`serial` 不会替你分配 id）：

```text
TABLE.DECLARE users PREFIX users: PK id COLUMN id i64 COLUMN email str COLUMN name str COLUMN plan str INDEX email unique
TABLE.DECLARE orders PREFIX orders: PK id COLUMN id i64 COLUMN user_id i64 COLUMN status str COLUMN total f64 COLUMN created_at i64 INDEX status range VALUES total created_at ORDERPATH user_id_created_at ON user_id THEN created_at DESC
TABLE.DECLARE order_items PREFIX order_items: PK id COLUMN id i64 COLUMN order_id i64 COLUMN sku str COLUMN qty i64 INDEX order_id range
VIEW.CREATE paid_orders QUERY orders.status EQ paid ORDER BY orders.status

# ---- query card: recent_orders_by_user ----
# runtime template — substitute the $N slots and send as-is:
#   $1 = user_id (i64)
#   IDX.QUERY orders.user_id_created_at WHERE user_id EQ $1 LIMIT 20 FIELDS id status total created_at

# notes:
#   - users.id: bigserial → i64, but ids do NOT auto-increment — allocate them app-side (INCR block, cookbook §3)
#   - orders.total: numeric → f64 — fixed-point precision becomes binary float; keep money as integer cents if exactness matters
#   - view paid_orders: read with VIEW.QUERY paid_orders, then hydrate rows with HMGET <key> id user_id status total created_at
```

行就是表前缀下普通的 hash（配方 1），而编译出来的路径立刻就能服务——把真实参数填进 `$1` 槽位，卡片就能跑：

```console
kevy-cli -p 6004 HSET users:1 id 1 email ada@example.com name Ada plan pro
kevy-cli -p 6004 HSET orders:1 id 1 user_id 1 status paid total 19.5 created_at 1700000100
kevy-cli -p 6004 HSET orders:2 id 2 user_id 1 status pending total 5 created_at 1700000200
kevy-cli -p 6004 HSET orders:3 id 3 user_id 2 status paid total 8 created_at 1700000300
kevy-cli -p 6004 HSET order_items:1 id 1 order_id 1 sku sku-7 qty 2
kevy-cli -p 6004 IDX.QUERY users.email EQ ada@example.com
kevy-cli -p 6004 IDX.QUERY orders.user_id_created_at WHERE user_id EQ 1 LIMIT 20 FIELDS id status total created_at
kevy-cli -p 6004 VIEW.QUERY paid_orders LIMIT 10
kevy-cli -p 6004 IDX.QUERY orders.status EQ paid FILTER total RANGE 10 inf
kevy-cli -p 6004 IDX.QUERY order_items.order_id EQ 1 FIELDS sku qty
kevy-cli -p 6004 TABLE.LIST
```

- 卡片那条查询是 `SELECT id, status, total, created_at FROM orders WHERE user_id = 1 ORDER BY created_at DESC LIMIT 20`——由复合走查服务，最新在前，一跳内补全字段。
- `FILTER total RANGE 10 inf` 那一行是在被 `INCLUDE` 的列上的**残余谓词**——`WHERE status = 'paid' AND total >= 10`，**不碰任何一行**。
- `order_items.order_id EQ 1` 就是那次取代 JOIN 的外键查找（配方 2）：两次查询，没有查询期 join。

**拒绝是有教的。**编译器拒绝一切需要查询期求值的东西——**具名**，带行/列，并指向那个替它建模的配方。一个 JOIN：

```sql
CREATE VIEW order_emails AS
  SELECT id, email FROM orders
  JOIN users ON users.id = orders.user_id;
```

```text
$ kevy-cli sql compile join.sql
kevy-cli sql: join.sql: line 6, col 3: JOIN is not compilable — kevy
refuses query-time joins (Law 3); model the lookup with an indexed FK
column (IDX.QUERY t.fk EQ …) or app-side assembly (cookbook §2)
```

一个 WHERE 匹配不到任何已声明访问路径的视图，会报错并**点名该加哪一条声明**（`… matches no declared access path — add: CREATE INDEX ON orders (status, total)`）；而运行期的 ad-hoc SQL，从来就没有过门：

```text
$ kevy-cli -p 6004 SQL SELECT * FROM users
(error) ERR unknown command 'SQL'
```

## Recipe 索引

Recipe ↔ 它替代的 SQL 构造 ↔ 陈述语义与边界的 [rds-workloads.md](rds-workloads.md) 矩阵行。

| # | Recipe | SQL 构造 | 矩阵行 |
|---|---|---|---|
| 1 | 表与行 | `CREATE TABLE`、点查 `SELECT` | [表、行、列](rds-workloads.md#表行列) |
| 2 | 一对多、多对多 | 外键列、关联表、`WHERE fk = ?` | [JOIN](rds-workloads.md#join) |
| 3 | 序列 | `AUTO_INCREMENT` / `nextval()` | [PK、UNIQUE、AUTO_INCREMENT](rds-workloads.md#primary-keyuniqueauto_increment) |
| 4 | 乐观锁 | 版本列 CAS `UPDATE` | [事务](rds-workloads.md#事务) |
| 5 | CHECK 约束 | `CHECK (…)` + 审计触发器 | [约束与触发器](rds-workloads.md#约束与触发器) |
| 6 | 幂等键 | `UNIQUE INDEX` + `ON CONFLICT DO NOTHING` | [PK、UNIQUE、AUTO_INCREMENT](rds-workloads.md#primary-keyuniqueauto_increment) |
| 7 | 软删除 | 标记列 + 过滤视图 | [VIEW](rds-workloads.md#view) |
| 8 | 复合排序 | `ORDER BY a, b` | [ORDER BY / LIMIT / OFFSET](rds-workloads.md#order-by--limit--offset) |
| 9 | JSONB | JSON 列 + 生成列索引 | [类型系统](rds-workloads.md#类型系统) |
| 10 | 级联删除 / 外键 | `ON DELETE CASCADE` | [约束与触发器](rds-workloads.md#约束与触发器) |
| 11 | 你不需要的 outbox | 事务性 outbox 表 | [CDC](rds-workloads.md#cdc) |
| 12 | 审计历史 | 审计表 / binlog 考古 | [CDC](rds-workloads.md#cdc) |
| 13 | 回滚窗口 | 切换期反向复制 | [迁移 playbook](migration.md) |
| 14 | 分析导出 | ETL / binlog tap 到数仓 | [CDC](rds-workloads.md#cdc) |
| 15 | 载入顺序 | 先批量 `LOAD DATA`、后建索引 | [二级索引 DDL](rds-workloads.md#二级索引-ddl) |
| 16 | 带 TTL 的会话上下文 | sessions 表 + 过期 cron | [容量估算与运维差异](rds-workloads.md#容量估算与运维差异) |
| 17 | 情景记忆 | 时间 `BETWEEN` + pgvector KNN | [SELECT](rds-workloads.md#select) |
| 18 | RAG 混合检索 | tsvector + pgvector，融合 | [SELECT](rds-workloads.md#select) |
| 19 | 传感器缓存 | upsert 表 + 陈旧度 cron | [容量估算与运维差异](rds-workloads.md#容量估算与运维差异) |
| 20 | 边缘聚合 | 每刷新一遍 `GROUP BY` + ETL 上行 | [GROUP BY 与聚合](rds-workloads.md#group-by-与聚合) |
| 21 | 派生状态作为行的函数 | 整个触发器层：级联、`UNIQUE`、对账 | [约束与触发器](rds-workloads.md#约束与触发器) |
| 22 | 移植一份 PG/MySQL schema | `CREATE TABLE` / `CREATE INDEX` / `CREATE VIEW`，编译过的 | [二级索引 DDL](rds-workloads.md#二级索引-ddl) |
