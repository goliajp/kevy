# 表（`TABLE.*` / `table_*`）

表是一份**具名的、可校验的声明**，在声明期就被编译成 kevy 已有的索引与视图原语。`TABLE.DECLARE` 接受一个前缀、类型化的列、二级索引和复合排序路径，产出普通的具名索引；查询期没有任何新东西在跑。这是引擎的常设规则（Law 3）的用户版表述：**kevy 从不为查询做规划——访问路径由你命名**，而表是把一整族访问路径一次性命名的省力写法。

```
TABLE.DECLARE user PREFIX u: PK id
    COLUMN id str COLUMN name str COLUMN age i64
    COLUMN dept str COLUMN email str
    INDEX age range VALUES dept name
    INDEX email unique
    ORDERPATH by_dept_age ON dept THEN age DESC

IDX.QUERY user.by_dept_age WHERE dept EQ eng LIMIT 20
```

行仍是前缀下的 hash 键，与从前完全一样——声明一张表不改变你写入的任何方式（`HSET u:1 name alice age 30 …`），也**不施加 schema**：缺少某个已声明列的行，就是该列为 NULL 的行（每个索引本来就有的缺字段语义）。这份声明为你买到的是编译好的访问路径、一个 `VERIFY` 面，以及对它们全体的单动词生命周期管理。

> **正在从手工维护的索引迁移？**先读 [table-migration.md](table-migration.md)——八条在生产上付过学费的经验，以及"表为什么存在"的实测漂移数字（89% 从未写入、76% 从未移除）。

## 声明模型

`TABLE.DECLARE` 把每个子句编译成一个具名索引：

| 子句 | 编译产物 |
|---|---|
| `INDEX <col> range\|unique [VALUES <col>…]` | 前缀上一个名为 `<table>.<col>` 的标量索引，`VALUES` 列按行存储（类型来自列声明） |
| `ORDERPATH <name> ON <col> [DESC] [THEN <col> [DESC]]…` | 一个名为 `<table>.<orderpath>` 的复合 range 索引——每行一个保序字节键 |

编译出的名字共用一个命名空间——`<table>.<col>` 与 `<table>.<orderpath>`——所以与被索引列同名的 ORDERPATH 在声明期就被按名拒绝。编译是服务端与嵌入式 store 共用的单一实现（dispatch oracle 在 CI 里对两个面做逐字节比对），并且是**原子的**：任何错误都导致什么也不装——不存在半声明的表。

编译出的索引做的一切，与手工 `IDX.CREATE` 声明的完全相同：同样的回填行为、同样的 `-INDEXBUILDING` 纪律、同样的 sidecar 持久化、同样的预算拒绝（[indexes.md](indexes.md)）。`TABLE.DROP` 删除表和它编译出的所有索引。

## 语法

```
TABLE.DECLARE name PREFIX p PK col
    COLUMN name i64|f64|str [COLUMN ...]
    [INDEX col range|unique [VALUES col ...]] ...
    [ORDERPATH name ON col [DESC] [THEN col [DESC]] ...] ...
TABLE.ENSURE ...       # TABLE.DECLARE 的开机形态——见下文
TABLE.REPLACE ...      # 显式的 drop + declare + 重建
TABLE.DROP name        # drops the table + its compiled indexes; 1|0
TABLE.LIST             # name/prefix/pk + column/index/orderpath counts
TABLE.VERIFY name      # component fsck + a bounded column spot check
```

## 开机模式：`ensure`

开机时声明才是常态——schema 住在你的代码里，每次进程启动都陈述一遍。`TABLE.ENSURE`（嵌入面：`Store::table_ensure`，返回 `TableEnsure::{Created, Unchanged}`）接受与 `TABLE.DECLARE` 相同的文法，是它的幂等形态：

- **表不存在** → 声明并构建：`Created`。
- **spec 完全相同** → 无操作的成功：`Unchanged`（线上回 `+UNCHANGED`）。此后每次开机都走这条路。
- **spec 不同** → **点名哪里变了的具名拒绝**（`COLUMNS` / `INDEXES` / `ORDERPATHS` / `PREFIX` / `PK`）——绝不静默重建。重建是对整个前缀域的全量回填，这么昂贵的事必须被点名要求：`TABLE.REPLACE`（嵌入面：`table_replace`）就是那个要求——显式 drop + declare + 重建，并在丢弃旧表**之前**先验证新 spec，坏的替换会让旧表继续服务。

裸的 `TABLE.DECLARE` 保持严格形态：重声明既有名字是错误。开机用 `ensure`，迁移用 `replace`，重名即 bug 的场合用 `declare`。

- 列类型是 `i64 | f64 | str`——即标量索引的类型。其余一切（时间戳、布尔、枚举）由应用编码进这三种之一，粗粒度映射被明说而不是被藏起来（kevy-sql 会对每个被转换的列打印一条说明）。
- `PK` 指向一个已声明的列；它是文档加一个 `VERIFY` 面——行仍按键寻址，和今天完全一样。`serial` 式的 id 分配是一份配方（[序列配方](cookbook.md#3-sequences)），不是引擎特性。
- 最多 64 张表；每一种结构性拒绝都有名字（重复列、未知的 `VALUES` 列、名字冲突……），从不静默。

`TABLE.VERIFY` **在调用那一刻、双向地现算每一个计数器**（4.1——此前 `coerce_failures` 是生命周期累计值，还把缺失列也吞了进去，没法和旁边现算的 `drift` 对读）：

- **index→row**：`entries` / `bytes` / `duplicates` / `drift` / `checked`——每条持有的 entry 都从它的行重新推导。
- **row→index**：前缀下的每一行按 cause 分类——走过的 `rows`、`coerce_failures`（存在但转换失败）、`excluded`（复合的 `str` 分量超 255 字节）、`absent`（分量列缺失：设计上的 NULL，不是错误），以及 **`missing`**（能推导出值却没有 entry 的行——忘了这张表存在的那个 writer，漂移走查在结构上看不见的唯一一类）。

一切正常时 `entries = rows − excluded − absent − coerce_failures` 成立；每种排除原因都有了自己的名字，而不是一个解释不了的 entries 差值。另有有界抽查（每 shard 最多 64 行）断言*出现了的*已声明列可转换。任何组成索引仍在回填时，回答 `-INDEXBUILDING`。

## 复合 ORDERPATH 语义

ORDERPATH 把[复合排序配方](cookbook.md#8-composite-ordering-order-by-a-b)——`ORDER BY a, b DESC` 的遍历——机械化成一个真正的复合索引：每行一条保序字节串，于是一棵 B-tree 就能像关系型复合索引那样回答查询。规则如下：

- **`WHERE` 取前导前缀。**`WHERE a EQ x [b EQ y …] [RANGE c min max]` 必须从头按声明顺序点名复合索引的列：一段等值前缀，然后在*下一列*上最多一个 range；其后全部不受约束（经典的复合 B-tree 语义）。点名一个非前缀列是具名错误——从不是一次扫描。
- `RANGE` 在 `WHERE` 内是终结的——它后面不能再跟任何东西，因为 range 之后的条件无法表示为一次连续遍历。
- **每个分量的 `DESC`** 体现在存储顺序里，所以 `ON dept THEN age DESC` 让每个部门的行从最老最大的那一端翻页，不需要重排。
- **缺少任一分量列的行被排除**在复合索引之外（转换失败同理）——它在其他所有访问路径上完全可见。长于 **255 字节**的 `str` 分量同样使该行被排除：这与关系型 B-tree 对索引行大小的同类上限一致，也是让 range 边界保持精确的前提。最多 8 个分量。
- `WHERE` 同样作用于 `IDX.COUNT`；在一个没有声明复合列的索引上使用它会被按名拒绝。
- **`TABLE.VERIFY` 的 `duplicates` 非零意味着这个 ORDERPATH 不是全序**——在所有分量上打平的行会塌缩成一条 entry，游标翻页会在平局边界跳行或重复。给复合的末尾加一个**有界的**决胜列（数值 id，或自然键的定宽哈希）——像裸 Message-ID 这样的无界字符串不会决胜，只会踩中 255 字节的排除上限。

```
IDX.QUERY user.by_dept_age WHERE dept EQ eng                  # all eng, age DESC
IDX.QUERY user.by_dept_age WHERE dept EQ eng RANGE age 31 46  # eng, 31<=age<=46
```

## 查询表

查询仍然是对编译名的 `IDX.QUERY`——表不新增查询动词，因为引擎在查询期不求值任何东西：

```
IDX.QUERY user.age RANGE 25 45                          # driving range
IDX.QUERY user.email EQ d@x                             # unique point lookup
IDX.QUERY user.age RANGE 0 100
    FILTER dept EQ eng SORT name ASC LIMIT 20 OFFSET 20 # clauses on VALUES
IDX.QUERY user.age RANGE 0 100 FACET dept
IDX.QUERY user.by_dept_age WHERE dept EQ eng LIMIT 20 FIELDS name email
```

`FILTER` / `SORT` / `DISTINCT` / `FACET` / `OFFSET` 读的是索引**在 `VALUES` 声明时存下的列**——与它们的全文检索原型相同的子句语法、相同的跨 shard 精确语义（[text-search.md](text-search.md)）：`FILTER` 在翻页之前生效，所以一条排得很深的合格行仍能进入 `LIMIT`；`FACET` 统计整个匹配集；缺失值在两个方向都排在最后。点名一个索引没有存储的字段是一个错误，错误里列出它存了哪些。驱动谓词永远是被索引的 range / EQ / WHERE——不存在没有索引的 `WHERE`。

**index-only 查询零行触达。**一条 FILTER / SORT / COUNT 查询完全从常驻 RAM 的索引作答——行读计数器在门禁套件里被断言 `== 0`（`bench/tablegate.sh`）。这正是这两个特性一起设计时瞄准的分层协同：开着[透明分层存储](tiering.md)时，一张全冷的表以**零磁盘读**服务 index-only 查询，只有最后的 hydration 页（`FIELDS …`）付冷读——每行一次、批量提交。不带 `VALUES` 列的索引在内存与查询路径上，与从未声明过这些列的 store 上的索引逐字节相同（零成本-未声明门禁）。

## NULL、唯一性，以及什么被强制

- **NULL = 缺失字段。**没有任何列是必填的；缺少被索引列的行只是不在那个索引里。没有引擎级 `CHECK`、默认值或 NOT NULL——约束是配方（[约束配方](cookbook.md#5-check-constraints-and-multi-key-invariants)，原子块）。
- 表层的**唯一性是校验而非强制**：`unique` 索引就是 `IDX.CREATE KIND unique` 建的那道围栏（[indexes.md](indexes.md#uniqueness-is-a-fence-not-a-lock)——预留模式让它免于竞态），`TABLE.VERIFY` 报告 `duplicates`，而不是引擎事后拒绝你的写入。

## 它不是什么

按拒绝来陈述，因为引擎是按名拒绝而不是勉强近似：**没有运行期 SQL**（对服务端发 `TABLE.DECLARE`，不是 `CREATE TABLE`），**没有查询期 join**（视图的 `VIA` 解引用除外，[views.md](views.md)），**没有 HAVING / 子查询 / 表达式**，**没有引擎强制的约束**。上述每一项的 SQL 到 kevy 映射在 [rds-workloads.md](rds-workloads.md)，可用的配方在 [cookbook.md](cookbook.md)，schema 编译路径见下。

## kevy-sql：编译 schema，而不是发送 schema

`kevy-sql`（及其 `kevy-cli sql` 面）是一个**声明期编译器**——像迁移工具一样，把一份 PG/MySQL 方言的 schema 文件读一次，产出显式声明：

```console
kevy-cli sql compile schema.sql                          # print the plan
kevy-cli sql compile schema.sql --apply --url 127.0.0.1:6004
```

- `CREATE TABLE` → `TABLE.DECLARE`（类型粗粒度映射到 `i64|f64|str`，每个映射都如实标注）。
- `CREATE [UNIQUE] INDEX` → `INDEX` 子句；PG 的 `INCLUDE` 覆盖列 → 存储的 `VALUES`；多列索引 → 一个 `ORDERPATH`。
- 常量的单表 `CREATE VIEW … AS SELECT` → 一个引擎视图；带参数的 → 一张**查询卡**：一条现成的 `IDX.QUERY` 模板，`$N` 槽位由你的应用运行时填入。
- 编译器同样不做规划：它把你的视图与你声明过的访问路径做匹配，匹配不上时告诉你该补哪条声明（`add: CREATE INDEX ON t (dept, age)`），而不是发明一次扫描。临时 SQL、join、子查询、`OR`、`GROUP BY` 等一律带 `line:col` 拒绝，并指向替代它的配方。

端到端的完整演练——一份真实的 users/orders/order_items schema 被编译、应用、查询——见[schema 迁移配方](cookbook.md#22-porting-a-pgmysql-schema)。

## 嵌入式

类型化 API，同一套编译，进程内不需要文本语法。声明类型——`TableSpec`、`TableIndex`、`OrderPath`——由门面重导出（4.1）：一切从 `kevy_embedded` import，永远不要依赖内部 crate。

```rust
use kevy_embedded::{TableEnsure, TableSpec};

match store.table_ensure(spec)? {    // 开机动词：验证、编译、同步构建
    TableEnsure::Created => {}
    TableEnsure::Unchanged => {}     // 同 spec 重启时无操作
}
let tables = store.table_list();
let report = store.table_verify_report(b"user")?;  // 具名的 fresh 计数
assert_eq!(report.per_index[0].missing, 0);        //   + 抽查
store.table_drop(b"user");
```

wire 形式（`db.cmd("TABLE.DECLARE", …)`）同样可用，用完全相同的共享语法解析——服务端 / 嵌入式的逐字节一致由 dispatch oracle 在 CI 里钉死。

## 性能

门禁钳制，及其测量状态照实说：一致性 / 一致对齐 / 拒绝 / index-only 断言在本树全部绿色通过（`bench/tablegate.sh`）。**吞吐钳制**——10 M 行下索引点查 p99 ≤ 1 ms、10 M 行下 FILTER+SORT+LIMIT-20 翻页 p95 ≤ 5 ms、3 个索引加已声明 VALUES 的写入税 ≤ 15 %（对比裸 `HSET`）——是 perfgate 指标行，其基线**待专用基准机记录**（`bench/capacity-envelope.sh` 负责记录）。在记录之前，它们是目标，不是测量结果——本页不会把它们当作结果引用。

写入成本是标准的索引税：每个编译出的索引对每次匹配写入付一次字段读加一次段更新；空目录的成本是一个不会命中的分支。

## 参见

- [indexes.md](indexes.md)——表所编译到的索引引擎。
- [tiering.md](tiering.md)——与表一起设计的另一半：索引热、行冷。
- [rds-workloads.md](rds-workloads.md)——完整的 SQL 词汇映射（什么可编译、什么是配方、什么被拒绝）。
- [cookbook.md](cookbook.md)——复合排序与 schema 迁移配方。
- [table-migration.md](table-migration.md)——从手工维护的索引迁移过来，八课。
- [views.md](views.md)——同一批索引上的具名组合。
