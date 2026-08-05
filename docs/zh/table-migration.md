# 从手工维护的索引迁移到表

这一章之所以存在，是因为一位生产环境的消费者完整做过这次迁移——一个邮件系统把应用代码里手工维护的二级索引（sorted set 与计数键）搬到了 `TABLE.DECLARE` 上——并把教训带了回来。这些教训属于引擎的文档，不属于他们的笔记本。下面每条规则都是付过学费的；排列顺序就是你将会需要它们的顺序。

**在这一切之前，是第一公里：**`kevy-cli sql plan schema.sql` 读你手上已经有的那份 schema，报告你的每一条查询会变成什么——各由哪条声明路径服务，服务不了的那些，则给出正好该加的那条 `CREATE INDEX`。它是*这东西到底搬不搬得动*的十分钟答案，而且不需要起服务。见 [tables.md](tables.md#kevy-sql编译-schema而不是发送-schema)。

## 先说为什么：只有引擎维护的索引才可验证

讲怎么做之前，先讲论据。当应用代码维护一个索引时，每一个 writer 都必须永远记得维护它，而没有任何东西在检查。上述迁移把手工维护的结构与引擎新建的索引对照，在一个维护良好的真实代码库里**实测**了这意味着什么：

- 某个手工索引缺了 **89%** 的行——后来新增的 writer 根本不知道这个索引存在（*从未写入*的漂移）。
- 另一个索引里 **76%** 的条目是陈旧的——删除路径删了行，却没删索引条目（*从未移除*的漂移）。
- 第三个索引成员一致、顺序却不一致——排序分数的公式只在其中一个 writer 里改过（*分数*漂移）。

这一切在应用内部都无从发现，因为索引本身就是"索引里该有什么"的唯一记录。引擎维护的索引在**类别上**不同：推导跑在写路径本身上，不存在会忘记的 writer——而 `TABLE.VERIFY` 随叫随到地双向现算（[tables.md](tables.md)）：index→row（`drift`）与 row→index（`missing`——恰好就是上面那类"被遗忘的 writer"，以一个数字可见）。这次迁移不是性能项目，是从*不可验证*到*可验证*的迁移。

## 八课，按需要的顺序

### 1. 第一问：每个查询维度在行上是单值的吗？

对每个想让索引回答的查询，问它的维度是不是**行上的**单一值。答案通常不在行里，而在你的 id 推导或键构造代码里——这个邮件系统的"每邮箱一线程"看起来单值，读了 id 代码才发现一个线程可以住在多个邮箱里。若维度是多值的，任何列都载不动它：为每个 (owner, item) 建一条**成员行**——`member:{owner}:{item}`，把 owner、item 和排序属性作为列——让 ORDERPATH 去排它。先决定这个，才不会事后重declare整张表。

### 2. 一旦开始供读，每个 writer 都在承重

派生行由写它的人填充。切读之前，**枚举所有 writer**——每一条创建、修改、删除底层实体的代码路径——确认每一条都在写这张表所声明的行。会忘的那个 writer，就是在表存在之前写下的那个。（这正是 `TABLE.VERIFY` 的 `missing` 计数事后能抓到的类别；审计是让你不必在生产上遇见它的办法。）

### 3. 回填要取"所有能点名条目的来源"的并集

遗留索引彼此不一致——这就是上面实测的 89% / 76%。从任何*单一*来源回填都会继承它的洞，而 `VERIFY` 看不见一条从未被写过的行。回填的键集合要从每一个能点名条目的结构（旧索引、主键空间扫描、归档）的**并集**构建，行的内容再从权威记录写入。

### 4. 切换前影子读——比内容，**也比顺序**

让读继续走旧路径，同时在旁边算出新答案并比较。不只比成员，还要比**顺序**：分数漂移产出的是顺序不同的同一集合，分页 UI 会把它变成用户可见的抖动。把第一处分歧连同**两边的排序键**一起写进日志——那一行日志立即点名漂移的 writer。

**`kevy-cli shadow` 替你做这件事。** 把两条命令都给它，它会比较两边产出的行键顺序，只要有分歧就以非零退出——所以切换脚本可以直接 gate：

```console
$ kevy-cli shadow -p 6004 \
    --old "ZRANGE old:act 0 -1 WITHSCORES" --old-pairs \
    --new "IDX.QUERY u.act RANGE 0 999 LIMIT 20" --samples 50
shadow: 50 samples, 50 diverged (first at sample 0)
  ORDER differs at position 0:
    old: u:5 (sort 5)
    new: u:1 (sort 10)
```

那一对排序值就是这一课说的那行。它报的另一种形状是 `MISSING`——旧路径有而新路径没有的行，也就是第 2 课**提前到来**：一个没人更新的 writer，在切换**之前**被看见，而不是事后由 `TABLE.VERIFY` 报出来。

有两件事它不猜。kevy 的分页回复（`[游标, [键, 排序值, …]]`）能从形状认出来，但 **member/score 对和平坦列表长得一模一样**——用 `WITHSCORES` 这类命令时要加 `--old-pairs`，否则每个分数都会被当成行键、每次采样都报分歧。另外**单次分歧是线索不是判决**：两侧是在同一条连接上背靠背读的，中间被写入的行会在这里现形。跑 `--samples n`，读那个比率。

### 5. 删旧结构：先枚举 reader，再删 writer

影子窗口关闭时，先移除旧索引的 **reader**，再移除 writer。相反的顺序有一种静默故障：读一个不存在的键返回 0 或空、不是错误——writer 都删光后残留的一个 reader，不会崩溃，只会静静地供出错误答案。之后再删存储的键。

### 6. 索引缺谓词 ⇒ 再加一条 ORDERPATH，永远不要复制列

当新查询需要一个现有形态给不出的谓词时，手工时代的反射是"把这个值再写到一个地方"——那是在重建这次迁移刚刚消灭的"两个 writer、一份真相"问题。改为在同样的列上声明**另一条 ORDERPATH**（或索引）；引擎会在同一次写里、从同一行推导出两者。

### 7. 开机用 `ensure`

常态就是[开机模式](tables.md#开机模式ensure)：每次进程启动 `TABLE.ENSURE`——第一次开机 `Created`，以后 `Unchanged`，而当代码里的 spec 与 store 里的不再一致时，得到**点名差异的拒绝**——那是让你主动跑一次 `TABLE.REPLACE` 迁移的信号，而不是被一次迁移撞上。

### 8. 让 `VERIFY` 成为运维的一部分，而不是迁移的一步

这些计数每次调用都是新鲜的，便宜到可以挂在 cron 或 doctor 命令里：`drift` 和 `missing` 应当永远为零；`absent` / `excluded` / `coerce_failures` 点名每种排除原因夺走的行（精确语义见 [tables.md](tables.md)，包括 ORDERPATH 的 `duplicates` 非零意味着分页需要一个有界决胜列）。整个迁移的意义就在于这些数字*存在*。去读它们。

**`kevy-cli doctor` 就是那个 cron。** 它对每张已声明的表跑 `VERIFY`，用退出码回答：

```console
$ kevy-cli doctor -p 6004
  OK       user  (rows 59999 · entries 59999 · absent 0 · excluded 0 · coerce_failures 0)
  WARN     ev    duplicates 1 — paging this path needs a bounded tie-break or pages repeat rows
  BUILDING new   — an index is still backfilling, not a verdict
doctor: 3 table(s) — 0 drifted, 1 warned, 1 still building
```

这套映射是本课自己的话，不是新的主张：`drift` 与 `missing` 非零**失败**；`duplicates` **警告**；`absent` / `excluded` / `coerce_failures` **只报告、永不失败**——每一种都是合法状态，一个会因为某列有 NULL 就报红的 doctor 会一直红下去。

两个刻意的选择。警告默认**不**导致失败——会因信息而失败的 cron 很快就没人读了——所以想要更严契约的人有 `--warn-is-failure`。而索引还在回填的表回的是 `-INDEXBUILDING`，那是**它自己的结局，不是失败**：把它当失败，就会在每次声明索引时叫醒某个人。

## 参见

- [tables.md](tables.md)——声明面、VERIFY 语义、复合 ORDERPATH 规则。
- [cookbook.md](cookbook.md)——序列、约束、复合排序的配方。
- [tiering.md](tiering.md)——行冷而索引热；index-only 查询零行触达。
