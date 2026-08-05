# 迁移——playbook 与工具链

分两半。**playbook**：如何把一个在线 RDS 负载分阶段搬上 kevy，每个阶段有自己的验证钳位与回滚点。**工具链**（`kevy-cli`）：把数据搬进、搬出、在 kevy 服务器之间搬运——并证明搬运是忠实的——的那组命令。建模问题（「我的 `JOIN` 变成什么？」）在 [rds-workloads.md](rds-workloads.md)；可跑的模式在 [cookbook](cookbook.md)。

## 分阶段 playbook

七个阶段。承重性质是：**直到写切换之前的每个阶段，RDS 始终是事实源，回滚就是「把应用指回去」**——永远不是「反向迁移数据」。不要跳阶段；每个阶段的验证钳位，正是让下一个阶段变得乏味的东西。

### 阶段 0——盘点

列出负载实际在跑的每张表和每种查询形状（事实源是你 ORM 的查询日志或 `pg_stat_statements`，不是 schema）。逐条对照 [rds-workloads 矩阵](rds-workloads.md)分类：大多数行能机械映射；标出撞上拒绝构造的那些（热路径上的 N 路 join、`HAVING`、深 `OFFSET` 分页、临时报表 SQL）。

- **动作**：一张成文的访问路径表：查询 → 矩阵行 → kevy 形状（index / view / agg / verb）。
- **钳位**：**在任何数据移动之前**，每条热路径查询都已有映射形状。拒绝构造现在就拿到应用侧的重设计决定，而不是拖到切换时。临时分析不上 kevy——改为规划仓库导出（cookbook recipe 14）。
- **回滚**：什么都还没动。

### 阶段 1——schema 映射

把盘点结果变成一份声明脚本：前缀、字段布局、类型，然后是每一行 `IDX.CREATE` / `VIEW.CREATE`。现在就应用矩阵里的类型规则——DECIMAL 转最小货币单位整数、DATETIME 转 epoch i64、NULL 转字段缺席——因为这些是写格式决定，回填之后再改代价高昂。

- **动作**：声明脚本，与你的 DDL 历史一起入库。
- **钳位**：拿几百行代表性数据在一次性 kevy 上跑一遍；`IDX.VERIFY` 必须显示 `coerce_failures = 0` 且 `duplicates` 符合预期；`VIEW.VERIFY` 无排序排除项；分页查询返回与 SQL 相同的结果。
- **回滚**：什么都还没动。

### 阶段 2——变更捕获（双写窗口开启）

在回填快照**之前**就开始捕获 RDS 写入，让缝隙里什么都掉不下去。两条路：

- **应用双写**——应用写 RDS（仍是事实源），同时把每笔写镜像到 kevy。简单、无新基础设施；但镜像骑在你的写路径上（做成 fire-and-forget 加失败计数器——丢失的镜像写靠重新回填修复，绝不阻塞 RDS 提交）。
- **CDC 拉取**——binlog/逻辑复制消费者（Debezium 或手搓）把行事件翻译成 kevy 写入。与应用解耦、按行有序、可从 binlog 位置重放；代价是连接器基础设施。改不动每个写入方、或写入方很多时优先选它。

无论哪条路，kevy 写入必须是**幂等重建形状**（`HSET` 整行、集合类 `DEL` + 重加），这样与回填重叠也无害。

- **钳位**：捕获路径上的新鲜度滞后指标（事件时间戳 → kevy 应用），加上每前缀的行数漂移检查（`PREFIX.STATS` vs `SELECT COUNT(*)`）收敛到常数。
- **回滚**：停掉捕获。RDS 毫无察觉。

### 阶段 3——回填

在已知捕获位置对 RDS 做快照，导出为 RESP 命令文件，批量载入：

```
# 从你的 RDS 导出生成重建帧（每行一条 HSET）：
#   HSET user:42 name ada email ada@example.com …
kevy-cli import -p 6004 --strict rows.resp        # ≥200k cmd/s
```

**先导入，后声明索引**（延迟索引规则）：回填以批量速度从既有行构建每个索引——比给每条导入行付一次写钩子便宜几个数量级。然后跑阶段 1 的声明脚本，等 `IDX.LIST` 报告 `state=ready`（细节见[载入带索引的 keyspace](#载入带索引的-keyspace)）。

- **钳位**：`PREFIX.STATS` 计数与每张表的 `SELECT COUNT(*)` 一致；`IDX.VERIFY` 的强转/重复计数是预期的零；抽样摘要检查——从 RDS 重算几百行、与 `HGETALL` 逐字节比对——通过。若中途经过一台中转 kevy，用 `kevy-cli diff` 证明这一跳。
- **回滚**：`kevy-cli delete-prefix`（或 `FLUSHALL`）后重来。RDS 仍是事实源。

### 阶段 4——读切换（按端点金丝雀）

逐端点翻转读流量，不搞 big-bang。为每个端点挑它需要的一致性阶梯级别（[availability.md](availability.md)）：浏览/列表类端点走第 0 级（异步副本读）；「我刚写的，得读回来」的流程用 `REPL.TOKEN`/`REPL.WAIT`（第 1 级）或直接读主节点。单节点 kevy 没有阶梯可想——每次读都是主节点。

- **动作**：先影子读（从 RDS 出答案，同时读 kevy，比对并统计不一致），再逐步调流量比例。
- **钳位**：一个完整流量周期（24h+）内不一致计数为零；服务延迟在你的 SLO 内。
- **回滚**：把读指回 RDS——即时生效，数据不动。

### 阶段 5——写切换 + 回滚窗口

把写入方翻到 kevy，并**在同一次部署里**启动反向镜像：一个 CDC 消费者把 kevy 的 feed 以 SQL 重放回 RDS（`FEED.READ` → `UPDATE`/`INSERT`，cookbook recipe 13）。此时 RDS 成了落后 kevy 几秒的温备；你的回滚方案是「把应用指回去」，不是「反向迁移」。

翻转之前，把序列 key 播种到 RDS 高水位**之上**（对新 key `INCRBY seq:order <rds_max_id>`）——经典的 auto-increment 冲突是阶段 5 最锋利的一条边。

- **钳位**：在 kevy 与镜像 RDS 之间做 `kevy-cli diff` 式抽样（对前缀做摘要、比对行样本）；再加上你本来就在告警的业务指标。
- **回滚**：镜像窗口内，把应用指回 RDS（接受镜像滞后——你正在测量它）。镜像消费者必须处理 `-FEEDRESYNC`（kevy 崩溃重启会递增 feed generation）：把受影响前缀重建进 RDS 后继续——at-least-once 加幂等 SQL 让这一步是安全的。

### 阶段 6——退役

信心固化之后（数周干净的 diff、一次演练过的故障切换），停掉反向镜像，给 RDS 做最后一次归档 dump，把运维故事完全搬到 kevy 上：快照 + AOF（+ feed 游标恢复点）照 [persistence.md](persistence.md)，副本/故障切换姿态照 [availability.md](availability.md)。

- **钳位**：在关掉 RDS **之前**——绝不是之后——用 kevy 自己的工件（快照 + feed 重放）演练一次恢复。

## 常见坑

- **DECIMAL 走了两条写路径。**双写路径存 `"10.00"`、回填存 `"1000"`（最小单位）的话，所有摘要检查都会失败，更糟的是读也会不一致。在阶段 1 定死格式，让两条路径共用一个序列化器。
- **auto-increment 冲突。**忘了把 `seq:*` key 播种到 RDS 最大 id 之上，就会发出已存在的 id。阶段 5 播种，开写之前用一句 `GET seq:order` 验证。
- **热 key。**一个 kevy key 恰好住在一个 shard 上；那行每个请求都摸的 RDS 行会变成单 shard 串行化点。把热计数器分片（按用户 key、块分配序列），读侧求和交给 `KIND agg`。
- **大 value。**几百 KB 的行能用，但会让每一跳都变重（导出帧、复制、feed 帧），并把服务成本推到网络路径上。热行保持精瘦；冷 blob 放对象存储、留一个指针字段，或把很少读的列拆进兄弟 key。
- **TTL 语义。**`EXPIRE` 给非正 TTL 会立即删除 key（Redis 契约）。导出的 TTL 以绝对 `PEXPIREAT` 期限传输，所以慢迁移不会延长寿命。摘要不看 TTL（只看值）——TTL 正确性靠抽样 `PTTL` 检查，不靠 `diff`。
- **`KEYS`/`SCAN` 的老习惯。**过去跑 `SELECT COUNT(*)` 的运维单行命令应改用 `PREFIX.STATS` 或 `IDX.COUNT`——`KEYS pattern` 是全 keyspace 收集、`SCAN MATCH` 是全量增量遍历；两者都不是服务路径（在大 keyspace 上连运维路径都勉强）。
- **索引构建与读竞速。**声明之后的一小段时间里，查询会回答 `-INDEXBUILDING`。切换窗口内的客户端需要轮询重试循环（见下文），或者在没有读者盯着的阶段 3 就把索引声明掉。

---

# 工具链（`kevy-cli`）

```
kevy-cli export  -p 6379 --prefix user: dump.resp
kevy-cli import  -p 6004 --strict dump.resp        # ≥200k cmd/s
kevy-cli import  -p 6004 --resume dump.resp        # 中断之后续传
kevy-cli digest  -p 6004 user:
kevy-cli diff    hostA:6379 hostB:6004 user: order:
kevy-cli copy-prefix   -p 6004 --rate 5000 user: staging:user:
kevy-cli delete-prefix -p 6004 --rate 5000 --dry-run tmp:
kevy-cli inspect -p 6004 user:
```

## 线格式

`export` 写出一条纯 **RESP 命令流**的重建帧——`DEL` + `SET`/`HSET`/`RPUSH`/`SADD`/`ZADD`，TTL 用绝对 `PEXPIREAT`。这让文件与 `redis-cli --pipe` 双向兼容：kevy 的导出能喂 Redis，任何 RESP 命令文件也能喂 `kevy-cli import`——包括你自己从 RDS dump 生成的那份（playbook 的阶段 3）。

每个 key 打头的 `DEL` 让重放**从零重建**——对它**发出的**每种类型都真正幂等（否则 RPUSH 这类追加 verb 会在重复导入时把列表内容翻倍）。

**但它并不发出每种类型。** 字符串、hash、list、set、zset 会重建，**流不会**——这里没有重建流的 verb，所以导出会把它们留下。这是这个工具的真实限制，不是你数据的问题，而且现在会**按名字报出来**：

```console
kevy-cli export: SKIPPED 1 key(s) of type 'stream' — nothing here
rebuilds that type, so they are NOT in this file
exported 4006 keys -> dump.resp
```

4.1 之前这个跳过是**静默的**：那个 key 被当成"遍历途中消失了"，于是带流的 store 导出得又干净又短。如果你的迁移里有流，请单独搬它们——并且在信任那个文件之前先看这行。

## 一致性与可续传性

- 导出是 SCAN 遍历下的按 key 时间点（SCAN 级保证；导出期间被写的 key 可能出现在任一状态，消失的 key 被跳过）。没有全局快照——有意为之。
- 导入按每批 512 条命令流水线化，每批之后把字节偏移 fsync 到 `<file>.progress`。`--resume` 从那里重启；因为帧是重建式的，重叠无害。`--strict` 在第一个服务器错误处中止；否则错误被计数并报告。
- 导入中途 `kill -9` 是有 gate 的场景：续传收敛到同一个 `PREFIX.DIGEST`。

## 验证

`PREFIX.DIGEST <prefix>`（服务器 + embedded 的 `prefix_digest`）返回 `[count, hex64]`——对规范化行字节的顺序不敏感校验和（hash 字段与 set 成员排序，zset 按 score 位再按成员，list 按顺序——list 的顺序就是身份）。它对 shard 数与插入顺序不敏感，所以能跨拓扑比较。`kevy-cli diff A:port B:port prefix…` 在任何不一致时以非零退出。

TTL 不参与摘要（它们会衰减）；值参与。

## 批量操作

`copy-prefix` 通过读取 + 重建帧把每行搬到新前缀下（服务器有意不提供 COPY verb；TTL 以绝对期限携带）。`delete-prefix` 走 SCAN + UNLINK。两者都接受 `--rate N`（令牌桶，从第一个操作起严格限速），删除还支持 `--dry-run`。

## 载入带索引的 keyspace

### 等待索引就绪

`IDX.CREATE` 立即返回，在后台回填；完成之前查询回答 `-INDEXBUILDING`。标准等法是轮询 `IDX.LIST`——它的 `state` 列从 `building` 翻到 `ready`：

```
until kevy-cli -p 6004 IDX.LIST | grep -A1 my_index | grep -q ready; do sleep 1; done
```

文本索引的回填速度随文档大小变化：小行约 7s/百万，而多 KB 的正文大约 85s/百万（实测：20 万封邮件大小的正文 17s）。

### 一条命令验证整场迁移

`kevy-cli diff` 一次调用即可跨两台在线服务器比较任意多个前缀——优先用它，而不是逐前缀的摘要对：

```
kevy-cli diff 127.0.0.1:6004 127.0.0.1:6005 msg: mbox: usr: tag: session:
```

### 大导出

dump 是未压缩的 RESP 文本（快、可 grep）。10GB+ 的 keyspace 用 gzip 管道——格式是流友好的：

```
kevy-cli export -p 6004 /dev/stdout | gzip > dump.kevy.gz
gunzip -c dump.kevy.gz | kevy-cli import -p 6005 --strict /dev/stdin
```

（`--resume` 需要真实文件来放 .progress 边车——要可续传就先解压到磁盘。）

索引在批量载入**之后**创建：索引引擎的回填以批量速度从既有数据构建（实测约 7s/百万行），胜过付一百万次逐写钩子维护。这是操作顺序，不是开关：

```
kevy-cli import -p 6004 dump.resp
kevy-cli -p 6004 IDX.CREATE users ON PREFIX user: FIELD age TYPE i64 KIND range
```

Gate：`bench/onrampgate.sh`（百万行往返、≥200k cmd/s 导入、kill -9 续传收敛、±20% 限速精度）。
