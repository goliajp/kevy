# V2 迁移演练门:真 PG 全链 → migrationgate(2026-08-08)

> ROADMAP V2:pg_dump → sql plan → backfill-keys → shadow → doctor
> 全链在**真 PostgreSQL 库**上演练,撞墙 finding 化并修;然后脚本化成
> `bench/migrationgate.sh` 进发布门。R4c 五件工具已在 develop;缺的是
> **那一天的工序被真库走一遍**。

## 形态决策(开工前定死)

1. **真 PG 在哪**:lx64 起**我们自己的** `postgres:18` 容器(独立
   port 15440,容器名 `kevy-migration-drill-pg`,演练毕即删——盒上
   两个 sentori-postgres 是别的租户,一个字节都不碰)。
2. **种子库 = vendored 生成器**,不是快照:`bench/migration-corpus/
   seed.sql`(schema)+ 确定性数据生成 SQL(generate_series,~50k
   行)。理由:可重复(gate 要求)、无脱敏问题(mailrs 语料 = P3
   拍板件,不等它)。
3. **schema 形状对着 R4a 实证**:三张表(users/messages/threads 形,
   bigint PK + text + timestamp + flag int)+ 二级索引若干 +
   **两个引擎拒绝的类型**(numeric money 列、inet 列)——"每种类型
   要么被搬走要么被点名"是章程 bar 的后半句,拒绝路径必须入戏。
4. **数据腿**:`psql COPY TO csv` → 演练脚本扮演 app(权威记录课:
   行写入是 app 的知识)→ HSET 帧 → `kevy-cli import`。转换器是
   演练脚本的一部分(水泥),不是新工具(lesson:工具猜权威记录 =
   自信地写错行)。
5. **比对腿**:行数 + 抽样字段对账(PG psql vs kevy TABLE 查询),
   `TABLE.VERIFY`/`IDX.VERIFY` 收尾;shadow 用于切换窗口(kevy 旧
   前缀 vs 新前缀)。
6. **migrationgate.sh**:一键全链,任何一步非零即 FAIL;末尾打印
   "搬走 N 类型 / 点名拒绝 M 类型 / 行数一致 / VERIFY 零漂移"。

## 切片

- **M1 种子语料(本地可做)**:`bench/migration-corpus/seed.sql` +
  期望文件;`sql plan` 过 schema,断言 served/refused 形状。
- **M2 盒上 PG 腿**:容器起停脚本化 + pg_dump 两段(schema/data)。
- **M3 数据腿**:CSV → 帧转换(演练脚本内嵌)→ import → 行数对账。
- **M4 验收腿**:TABLE.VERIFY / IDX.VERIFY / doctor / lint 全绿断言。
- **M5 gate 化**:migrationgate.sh 串起 M2-M4,trap 清容器,
  重复跑两遍验证可重复性。
- **M6 finding**:撞墙逐条入 `bench/FINDING-*.md`;工具缺口修小的、
  finding 大的(平衡轮纪律沿用)。

## 纪律

- 盒上自己的容器自己清(trap + 收尾双保险);sentori-* 不碰。
- 每步产物落 `$HOME/captmp`(非 tmpfs 教训);gate 内 mktemp。
- 撞墙即 finding,不静默绕。
