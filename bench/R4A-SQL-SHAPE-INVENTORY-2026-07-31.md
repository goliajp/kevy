# R4A — SME RDS 业务真实 SQL 形状清单(四源实证)

Read-only 研究,2026-07-31。为"虚拟 SQL support"方向提供地基:**真实 SME 业务打到存储引擎上的
SQL 形状分布**,只陈述四个证据源能支撑的,不引教科书假设。

## 证据源

| 源 | 是什么 | 证据性质 |
|---|---|---|
| **S1 mailrs** | 生产邮件系统,14 条线程查询轴已迁 kevy TABLE 层 | 迁移实录(dogfood note)+ 现行代码 grep 取样 |
| **S1' mailrs-on-PG** | 同一系统迁 kevy **之前**打给 PG/spg 的 prod SQL | spg `xtests/dogfood_replay/fixtures/mailrs-*` 四条真实 prod 事故 SQL |
| **S2 smix** | iOS 自动化工具,kevy-embedded 3.18 嵌入方 | `smix/crates/smix-store/src/lib.rs`(629 行,全读) |
| **S3 kevy docs** | kevy 自己的 SQL 对应矩阵 | `docs/rds-workloads.md` 全文 + `docs/tables.md` §What it is NOT + `docs/cookbook.md` 22 recipes |
| **S4 spg** | 姐妹项目,纯 Rust PG-dialect RDS,见过真实 SQL 负载 | `spg/README.md` §What it ships + `xtests/sqllogictest/corpus/`(89 条 pg_regress 特性探针)+ dogfood_replay |

文件基点(引文全部来自今天实际读取):
- dogfood note = `stables/mailrs/.claude/notes/kevy-v4-dogfood-feedback-2026-07-26.md`(下引 F#)
- mailrs 代码 = `stables/mailrs/crates/mailbox-kevy/src/`
- smix = `goliajp/smix/crates/smix-store/src/lib.rs`
- spg = `goliajp/spg/`

---

## 清单表

kevy 现状四值:**已有(面)** / **引擎缺** / **配方 §n**(cookbook)/ **拒绝→替代**。
SME 分量依据写在括号里。

### A. 读形状(单表)

| 形状 | 证据 | kevy 现状 | SME 分量 |
|---|---|---|---|
| 单表主键点查(`WHERE pk = ?`) | S2 `Namespace::get` lib.rs:411 / `get_json`:460(smix 的读几乎全是它);S1 `hydrate_page` 逐 tid HGETALL;S3 rds-workloads §Tables | 已有(`GET`/`HGETALL`/`HMGET`) | **高**(两个嵌入方的主读路径) |
| 二级索引等值(`WHERE col = ?`) | S1 alias.rs:132 / domain.rs:59(`idx_query` on by_domain/by_target);S1 lib.rs:255-315 六个 Range 索引全按等值形状用 | 已有(`IDX.QUERY EQ` / range 索引等值边界) | **高** |
| 二级索引范围+排序+分页(list page) | S1 dogfood "Status: cutover complete" 表:14 轴全部;lib.rs:1045-1076 `run_orderpath`(composite bounds → `idx_query`);S1' content-worker fixture `ORDER BY m.id DESC LIMIT 64` | 已有(ORDERPATH + `IDX.QUERY RANGE` + cursor) | **高**(mailrs 的全部 UI 列表) |
| 等值前缀 + 范围尾(`WHERE a=? AND b=? AND ts<?`) | S1 lib.rs:697 `list_thread_ids_by_bucket_before_via_table`("activity 是复合中紧跟等值列之后的分量——唯一可加 range 的位置") | 已有(复合 ORDERPATH 的设计形状) | **高**(游标翻页的实现基础) |
| ORDER BY 多列(复合排序) | S1 lib.rs:415-460 四条 orderpath(user→bucket→activity desc→ord);S1' track-a `ORDER BY BOOL_OR(m.pinned) DESC, MAX(m.internal_date) DESC` | 已有(ORDERPATH 多列;bare 索引层需自拼复合字段,配方 §8) | **高** |
| ORDER BY DESC | S1 orderpath 每条都 `("activity", true)`(desc);S1' 四条 fixture 全部 DESC | 已有(ORDERPATH desc flag / VIEW `ORDER BY … DESC`;bare `IDX.QUERY` 无 desc) | **高**(newest-first 是列表默认) |
| COUNT(`COUNT(*) WHERE …`) | S1 lib.rs:675-688 `idx_count` + 全文 5 处;S1' class-b fixture 整条就是 COUNT;F17 三个坏掉的计数端点(badge/tabs/uidnext) | 已有(`IDX.COUNT`) | **高**(badge/tab 计数是 UI 刚需) |
| 带残余谓词的 COUNT | S1 lib.rs:922 注释原话 "`idx_count` takes no clauses, so this counts the rows a clause query returns" —— 消费者被迫物化行再数 | **引擎缺**(claused count 不存在,靠 `idx_query_claused` 取行数) | **高**(mailrs 每个 stacked 轴都要) |
| FILTER/SORT/OFFSET on 存储列(stacked 谓词) | S1 lib.rs:894-905 `idx_query_claused`(Eq filters + sort activity desc + offset);声明注释:"The UI stacks constantly — archived within Inbox" lib.rs:348 | 已有(`IDX.QUERY … FILTER/SORT/OFFSET`,tables.md §Querying) | **高** |
| 布尔 flag 谓词(`WHERE starred=1`) | S1 lib.rs:395-407 六个 flag 索引(starred/archived/pinned/unread/has_action/is_sender);S1' `(m.flags & 1) = 0` 位运算形状 | 已有(i64 0/1 索引;位运算谓词无——PG 侧用 bitmask,kevy 侧拆成独立字段,mailrs 迁移时正是这么拆的) | **高** |
| IN 列表(`col IN (v1,v2)`) | S1' class-b `ea.category IN ('spam','scam')`;S3 rds-workloads §SELECT | 已有(`COMPOSE OR` 两腿;>2 腿 = N 次 EQ app 侧并) | 中 |
| OR/UNION 合并轴 | S1 list_threads.rs:741-776 `list_np_via_table`(两 ORDERPATH 页 app 侧 merge+resort);同文件 273 `zunionstore` 遗留路径 | 已有但打折(`COMPOSE OR` 限两索引;跨 ORDERPATH 的 union 是 app 侧手工 merge) | 中-高(mailrs 真实轴 "N & P") |
| AND 交集 | S1 list_threads.rs:290 `zinterstore` 遗留;现行由 FILTER on VALUES 取代(同一索引内) | 已有(`COMPOSE AND` / FILTER) | 中-高 |
| LIKE 前缀 | S3 rds-workloads §SELECT(str range 索引前缀边界);S2 `collect_keys(Some(pattern))` lib.rs:492 是键名前缀版 | 已有(range 索引 `RANGE 'abc' 'abc\xff'`) | 中(业务代码里只见键名前缀列举) |
| LIKE 中缀 + LOWER | S1' class-c `LOWER(COALESCE(…)) NOT LIKE '%'\|\|LOWER(addr)\|\|'%'`(prod 真用) | 拒绝→替代(token 形状走全文 MATCH;否则重设计字段,rds-workloads §SELECT) | 中 |
| 全文检索 | S1 rethread.rs:409-425 `idx_match`(BM25,CJK bigram)+ lib.rs:262-281 两个 Text 索引(thread search_blob + message bodies);S4 corpus `22_fulltext_index.test` | 已有(`KIND text` + `MATCH`) | **高**(mailrs 会话搜索是主功能,曾因索引失联"dead for weeks"——lib.rs:259 注释) |
| 全键空间/前缀列举(admin scan) | S2 `raw_keys` lib.rs:385 + `Namespace::list`:488;S1 account.rs:93-105(range 索引全值域扫,列所有账号) | 已有(`SCAN MATCH` / `collect_keys` / 索引全域 range) | 中(运维/admin 面,非 serving) |

### B. 读形状(多表/关系)

| 形状 | 证据 | kevy 现状 | SME 分量 |
|---|---|---|---|
| JOIN 一对多 | S1' 四条 fixture 全有 `JOIN mailboxes mb ON m.mailbox_id = mb.id`;S1 迁移后:membership row + `hydrate_page` 两跳 | 拒绝→替代(denormalize / `FIELDS` 水合 / `VIA`,rds-workloads §JOIN;配方 §2) | **高**(PG 时代每条 prod 查询都在 join) |
| 多对多(membership) | F6a:thread 多 owner ⇒ 表声明在 `mailrs:threaduser:{user}:{tid}` membership row 上——"the ordinary relational answer to a many-to-many" | 已有模式(配方 §2;F6a 证明它是迁移的**第一个**要回答的问题) | **高** |
| LEFT JOIN | S1' track-a `LEFT JOIN email_analysis ea ON ea.message_id = m.id` | 拒绝→替代(denormalize;缺失即字段缺席 = NULL 语义) | 中-高 |
| 反连接(NOT EXISTS) | S1' class-b、class-c、content-worker 三条 fixture 都是 correlated NOT EXISTS(spam 过滤 / snooze 过滤 / 未处理附件) | 拒绝→替代(写时物化排除 flag 进行/索引——mailrs 迁移后 spam 判定就是写进 bucket 字段的) | **高**(prod 事故 SQL 的公共形状,4 条里 3 条) |
| 标量子查询 / greatest-n-per-group | S1' track-a 三处 `(SELECT … ORDER BY internal_date DESC LIMIT 1)`(每线程最新 category/snippet/sender) | 拒绝→替代(写时把 latest 冗余到行上,配方 §21 派生态);tables.md §NOT 明拒 subqueries | **高**(每组最新一行是邮件/订单类 UI 的通用形状) |
| GROUP BY 聚合 | S1' track-a `GROUP BY m.thread_id` + 15 个聚合列;S3 rds-workloads §GROUP BY | 部分已有(`KIND agg`: count/sum/min/max/avg,写时折叠;`FACET` 分组计数)——track-a 那种 BOOL_OR/string_agg/array_agg/COUNT(DISTINCT CASE) **引擎缺** | 中-高 |
| HAVING | S1' track-a/class-c 的 `HAVING BOOL_OR(…)=false AND COUNT(CASE …)>0` | 拒绝→替代(app 侧过滤 GROUPS 输出;tables.md §NOT) | 中 |
| CTE(WITH) | S1' class-c(且 mailrs 注释自述:这已是给 spg 7.30.3 降级改写过的形状,"paying a readability tax");S4 README:CTE + WITH RECURSIVE 全支持 | 拒绝(无查询语言) | 低-中(出现即事故级慢查询,但只 1 条) |
| 窗口函数 | S4 README 支持 + corpus `77_window_join.test`;**业务侧四源零出现**(track-a 用 array_agg[1]/标量子查询代替) | 拒绝 | 低(见反直觉 #4) |
| FACET / 分组计数(tabs) | F17:category tabs 计数端点(曾从遗留 zset 读坏);S3 tables.md `FACET dept` | 已有(`FACET`,数整个匹配集) | 中 |

### C. 写形状 / 一致性

| 形状 | 证据 | kevy 现状 | SME 分量 |
|---|---|---|---|
| 单实体原子 RMW(读-判-写) | S1 15+ 处 `.atomic(\|ctx\|…)`:account.rs:54,156 / alias.rs:92,111,184 / domain.rs:23,39 / mutations.rs:89+ / list_threads.rs:341(整页水合进一个闭包防写者插入) | 已有(embedded `atomic`;server 侧 `EVAL`/`MULTI`) | **高**(mailrs 一切写路径) |
| 多语句交互式事务(BEGIN…读…写…COMMIT) | **四源业务代码零出现**;S4 支持(savepoints、FOR UPDATE probe)但 dogfood fixture 无一用到 | 拒绝→替代(WATCH/CAS、Lua、atomic;rds-workloads §Transactions) | 低(见反直觉 #1) |
| 乐观锁(version 列) | S3 配方 §4;S4 corpus `27_for_update.test`;mailrs 未用(atomic 已覆盖) | 已有模式(WATCH+MULTI) | 低-中 |
| UPSERT | S1 `upsert_thread`(F6 引用的写路径名);S2 `put` 语义即 replace | 已有(`HSET`/`SET` 天然 upsert) | **高**(KV 语义下不成为独立形状) |
| UNIQUE 约束 | **业务零使用**:S1 六个索引全 `IndexKind::Range`+Text,无 unique;S2 无 | 已有(fence 语义 + `SET NX` 硬门,rds-workloads §UNIQUE) | 低(见反直觉 #2) |
| FK / CASCADE delete | F15:`delete_thread` 差点把 membership row 留成孤儿——级联是**手工**清的;S3 配方 §10 | 拒绝→替代(atomic 块内父子同写 / CDC 消费者) | 中(不 enforce 但级联清理是真实义务) |
| CHECK 约束 | 业务零使用;S3 配方 §5 | 拒绝→替代(atomic/Lua 内评估不变量) | 低 |
| 序列 / AUTO_INCREMENT | **业务零使用天然键代替**:S1 tid = Message-ID(F6a 引 fastcore 推导代码)、账号键 = email;S2 键 = UDID/设备名;S4 corpus `16_sequences.test`(兼容面有) | 已有(`INCR` 块分配,配方 §3) | 低-中(见反直觉 #3) |
| 软删除 | S1 `archived` flag 索引(形状上就是软删除轴);S3 配方 §7 | 已有模式(flag 字段 + 索引) | 中 |
| TTL / 过期 | S1 list_threads.rs:276,293(temp 键 60s 自清);S3 配方 §16、rds-workloads(RDS 侧是 cron delete) | 已有(`EXPIRE`/`HEXPIRE`,一等公民) | 中-高(反向形状:RDS 没有的原语被真实用了) |
| 幂等键 | S3 配方 §6;业务侧未取到样 | 已有模式(`SET NX` + TTL) | 悬(入悬案区) |
| 触发器 / CDC | S3 配方 §10-12(`FEED.READ`);S1 未见调用 | 拒绝触发器→替代(CDC 消费者) | 悬(入悬案区) |
| 审计历史 | S3 配方 §12;S4 自带 BLAKE3 hash-chain audit(README §What it ships) | 已有模式(CDC 消费者落审计) | 低-中 |

### D. 派生/运维形状

| 形状 | 证据 | kevy 现状 | SME 分量 |
|---|---|---|---|
| 索引一致性校验 | F8/F11/F12:`TABLE.VERIFY` + shadow read 抓出手维护索引三类静默腐坏(89% 漏写 / 76% 漏删 / score drift)——dogfood 称其为 TABLE 层最强论据 | 已有(`TABLE.VERIFY`/`IDX.VERIFY`:drift/duplicates/coerce_failures) | **高**(SQL 引擎里没有对应物——它是 kevy 独有的形状) |
| 排序总序(分页正确性) | F8:activity 秒级时间戳 929 处并列 → 分页跳行/重行;修法 = pk hash 进 orderpath 尾;F9:>255B 字符串分量整行落索引 | 已有(ORDERPATH 多列 tie-break;`duplicates` 计数暴露非总序) | **高**(教科书不讲、prod 必踩) |
| 物化视图 / hot list | S3 views.md TOPK;S4 corpus `18_materialized_views.test`;**mailrs 零使用**(ORDERPATH 已覆盖其需求) | 已有(`VIEW.* MODE materialized TOPK`) | 悬(入悬案区) |
| 向量 / ANN | S4 pgvector corpus 是 **P0 100% 目标**(corpus README 表);S3 配方 §17/18(episodic memory、RAG hybrid) | 已有(`KIND ann`) | 中(业务实证只有 spg 的兼容优先级作旁证) |
| JSONB / 文档字段 | S4 corpus `34_jsonb_path_query.test`、`61_json_build.test`;S2 值全是 JSON blob 但**整存整取**从不服务器侧查 | 拒绝 json-path→替代(拍平成 hash 字段,配方 §9) | 中 |
| DECIMAL / money | S4 corpus `68_pg_money.test`;S3 rds-workloads 类型表 | 拒绝→替代(整数最小单位) | 中 |
| 日期时间函数/算术 | S4 corpus 8 条探针(06-13:date_time/date_functions/now_and_date_arith/interval/timestamptz…);S1' `NOW()`、`snoozed_until > NOW()` | **已有(2026-08-03 R4a-time)**:查询 bound `@` 表达式(`@now±<n>s/m/h/d/w/mo/y`、`@YYYY-MM-DD[Thh:mm:ss]`,kevy-time 石头,RANGE/EQ/WHERE/FILTER 全 bound 面双面);timestamptz/时区 app 侧(具名拒);date_trunc 分桶 = RFC 发散区 | 中-高(兼容面大头之一) |
| 标量函数面(字符串/数学) | S4 pg_regress 89 条里约 40 条是纯标量函数(concat/trim/replace/split_part/lpad/strpos/left_right/translate/floor/ceil/round/mod/power/sqrt/nullif/greatest/least/uuid/format/regexp…);S1' COALESCE/LOWER/LEFT/CAST 满地 | 引擎缺(全 app 侧) | 见反直觉 #5——按兼容工作量计**高**,按 kevy 模式计不适用 |
| 备份 / PITR | S3 rds-workloads §Backup(snapshot+AOF+recovery-point);S4 README backup/restore/retention 命令行 | 已有 | 中(运维必备,非查询形状) |
| 集合成员(tags) | S2 `Set::add/remove/members` lib.rs:584-628("which sims are capturing right now") | 已有(SADD/SREM/SMEMBERS) | 中 |
| 批量导入后建索引 | S3 配方 §15(deferred-index rule);F14 backfill 教训(枚举源必须取并集) | 已有(在线 build + `-INDEXBUILDING`) | 中 |

---

## 悬案区

- **CDC/FEED 的真实使用** — cookbook 三条配方(§10-12)+ "outbox 不需要"的强主张,但两个真实嵌入方都没取到一处 `FEED.READ` 调用;mailrs dogfood 通篇未提 feed。主张有、实证无。
- **VIEW.\* 的真实需求** — mailrs 14 轴全部由 ORDERPATH + flag 索引覆盖,一个 VIEW 都没声明;smix 无。"hot list 是 killer app"(views.md)目前只有文档自证。可能 TABLE/ORDERPATH 落地后 VIEW 的地盘被吃掉了——值得在 v5 方向上重新问。
- **幂等键(配方 §6)** — 模式合理,但四源没取到业务调用样本;mailrs ingest 的去重靠 Message-ID 天然键,不是显式幂等键。
- **smix 早年那张 postgres 表** — lib.rs:3-8 头注释说迁移前有"a valkey set and a postgres table";那张表装过什么形状已不可考(代码已删),无法计入分布。
- **mailrs zset 遗留路径是否仍在 serving** — list_threads.rs:255-340 的 zunion/zinter/zrevrange 路径(注释标 v2.9)与 `*_via_table` 路径并存于同一文件;dogfood 说 cutover complete,但今天只做了静态读,没验证运行时走哪条。两种可能(留作 fallback / 待删死代码)都没证据拍死。
- **`np` 轴的合并语义归属** — dogfood 状态表写 "np = merge of two ORDERPATH ranges",实现是 app 侧取两页、合并、重排、再 offset(list_threads.rs:741-776)。这是"引擎缺跨索引 OR+排序"的证据,还是"app 侧合并本来就够"的证据,取决于轴的规模上限——没有数据。
- **位运算谓词**(`(flags & 1) = 0`)— PG 时代真实用了 bitmask 列;mailrs 迁 kevy 时拆成了独立 i64 字段。这算"拒绝且替代已被真实走通",还是算引擎缺,是归类争议——上表按前者归了,记录在此。

---

## 反直觉发现

1. **交互式多语句事务在真实业务里零出现。** 教科书把 `BEGIN…COMMIT` 当 RDS 的核心卖点;四源里 mailrs 的全部写路径是 15+ 处单实体 `atomic` 闭包,smix 连事务概念都没有,spg 的四条 prod 事故 SQL 全是只读 SELECT。真实 SME 的原子性需求形状 = "一个实体上的读-判-写",不是跨表编排。
2. **引擎约束(UNIQUE/FK/CHECK)业务零使用。** mailrs 六个索引全是 Range/Text,没有一个 unique;唯一性靠天然键(email、Message-ID)天然成立。但 F15 表明**级联清理的义务是真的**(delete_thread 差点留孤儿 membership row)——SME 需要的不是 enforce,是"删父时别忘了子"的工序。
3. **SERIAL/序列零使用——天然键统治。** mailrs 的 tid 是 Message-ID(F6a 专门读了推导代码确认),账号键是 email;smix 的键是 UDID/设备名。教科书必备的 auto-increment 在两个真实嵌入方里一次都没出现。
4. **窗口函数:引擎支持、业务零用。** spg 完整支持 OVER(PARTITION BY…),但同一业主的真实 prod SQL 在需要"每组最新一行"时写的是 `(array_agg(x ORDER BY ts DESC))[1]` 和相关标量子查询 LIMIT 1(track-a 三处)——教科书答案 ROW_NUMBER() 没人写。greatest-n-per-group 这个形状本身倒是高频,kevy 的写时冗余(denormalize)恰好是它的 O(1) 答案。
5. **PG 兼容工作量的大头是标量函数,不是关系代数。** spg 的 89 条 pg_regress 特性探针里约 40 条是 LOWER/COALESCE/split_part/round 这类标量函数,再加 8 条日期时间——关系代数(join/子查询/窗口)只占小头。任何"虚拟 SQL"面若想吃真实负载,函数面是绕不开的隐性成本。
6. **真实 prod 慢查询事故 100% 是读聚合。** spg 的四条 mailrs 事故 fixture(3.6s~19s 级)全部是 SELECT:COUNT+反连接、dashboard CTE、大 GROUP BY 会话列表。写路径没有出过一条事故 SQL。kevy "把聚合搬进写路径"(KIND agg / ORDERPATH)正对着事故分布打。
7. **OFFSET:文档拒绝、引擎提供、消费者真用。** rds-workloads §What kevy will NOT do 明列 OFFSET 拒绝;但 tables.md 的 claused 查询面有 `OFFSET`,mailrs 真实用了两处(`ScalarQueryOpts.offset`、np 合并后手工 drain(offset))。拒绝声明与 TABLE 层演化已经不一致——文档或引擎有一边该改口。
8. **排序总序是教科书不讲的第一分页坑。** F8:秒级时间戳在 30k 行上撞出 929 处并列,并列即分页跳行/重行;F9:修 tie-break 又踩 255B 分量上限把两行整行踢出索引、进而 panic 拖垮 prod。SQL 世界里 ORDER BY 非总序同样静默——但没有 `duplicates` 计数器暴露它。这是 kevy 面比 SQL 面**更诚实**的一处。
9. **一个生产嵌入方的"SQL 形状需求"可以为零。** smix-store 629 行:点查、前缀列举、集合成员、fsync,连 hash 都没用,值是整存整取的 JSON blob。SME 工具类负载的下界就是纯 KV——虚拟 SQL 面对这类用户是零价值,分层收费/分层文档时值得记住。
10. **手维护索引在 prod 上必然腐坏——且三种坏法全中。** F11/F12:89% 的 spam 判定没写进 UI 读的索引、76% 的重分类没删旧条目、同成员不同序(score drift)。这不是 mailrs 手艺问题,是"无法被校验的东西必然漂移"——任何鼓吹 app 侧自维护二级索引的方案(包括教科书的 Redis 惯用法)都该按此定价。

---

*方法说明:S1 代码为取样非穷尽(lib.rs / account / alias / domain / list_threads / rethread / mutations 的调用点 grep + 关键函数细读);S2 全读;S3 三个文档按任务指定范围读;S4 读 README、corpus 组织、四条 dogfood fixture 全文,未读 8404 行 AST 全文(以 README §What it ships 的自述面为准)。所有 file:line 均来自 2026-07-31 实际读取。*
