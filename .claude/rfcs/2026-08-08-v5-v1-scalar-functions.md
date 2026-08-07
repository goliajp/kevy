# RFC: V1 标量函数面 — kevy-scalar 石头 + sql 面求值,引擎零改动

> V1 train(工业化章程 §2.2):R4a 清单(~40 标量 + 8 日期时间)实现,
> funcgate 覆盖率报告。本 RFC 定四件事:bar 重锚、求值落点、
> kevy-scalar 的形状、funcgate 的操作定义。

## 0. Bar 重锚(拍板点 ①,先说丑话)

章程写的是「**89 探针集 served ≥ 80%**」。按探针逐个分类(89 个
`.test` 文件 = spg `xtests/sqllogictest/corpus/pg_regress/`),这个数
**不可达**:

| 类 | 探针 | 数量 | kevy 的答案 |
|---|---|---:|---|
| 纯标量函数 | 32-33, 36-58(regexp/format/concat/trim/replace/split_part/repeat/lpad/strpos/left_right/floor/ceil/round/trunc/nullif/greatest_least/mod/power/sqrt/sign/random/translate/uuid) | 25 | **本弧服务** |
| 日期时间 | 06-13, 38, 62, 65-67(date_time/date_functions/now/interval/timestamptz/bare now/mysql alias/pg_time/year/timetz) | 12 | **本弧大部服务**(timestamptz/timetz 维持具名拒,app 侧时区) |
| 已有面覆盖 | 01-02(DDL/INSERT)、17(单表 view)、22(fulltext)、29-30(generate_series 拒/limit)、75(FETCH…TIES)、88(boolean)、90(wire text) | ~8 | 部分已 served,部分本来就拒 |
| 关系代数/事务 | 03, 05, 27-28, 76-79(MERGE/LATERAL/window join/setof/savepoints/for update/deferrable) | 8 | 永久具名拒(Law 3 / 事务模型) |
| 类型异域 | 04, 19-20, 25, 31, 34-35, 68-74(enum/domain/collate/regtype/jsonb_path/inet/money/mysql enum·set/range/hstore/2d array/inet contains) | 14 | 永久具名拒→替代(R4a 表既有结论) |
| 目录/会话仿真 | 15-16, 18, 21, 23-24, 26, 59-61, 63-64, 80-87(pg_catalog 八视图/序列/matview/schema/超时/appname/string_agg/bool_agg/json_build/session/typeof) | 22 | 永久具名拒(仿真面 = scope-decisions OUT) |

天花板 ≈ (25+12+8)/89 ≈ **50%**。「89×80%」是 L4 finding 点名的那类
「测量存在前写下的句子」——R4a 原文是「89 条**里约 40 条**是标量函数」,
章程转写时把分母吃进了口径。

**重锚建议**(候选,拍板归属主):

* **B-1(推荐)**:双线 —— ①函数子集(上表前两类 37 探针)
  **served ≥ 80%**(≥30/37);②全 89 探针 **100% 分类可答**
  (served / named-refusal-with-alternative,零静默失败),`sql plan`
  输出这张表本身。既守住实现量,又把「边界诚实」变成门禁断言。
* B-2:维持 89×80%,把仿真面(目录视图/会话)做成假答案 —— 违反
  scope-decisions(PG wire 仿真 OUT),不推荐。
* B-3:bar 只写函数子集,89 全表仅报告不断言 —— 比 B-1 少一条真断言。

## 1. 设计输入(全部实证)

1. **R4a 反直觉 #5**:PG 兼容工作量大头 = 标量函数,不是关系代数。
   S1' 证据:mailrs 的 SQL 时代 `COALESCE/LOWER/LEFT/CAST` 满地。
2. **Law 3**(ROADMAP 三定律):meaning 和 planning **永不进引擎**。
   kevy-sql 现状与之一致:声明期编译器,表达式全部具名拒。
3. **探针语料的形状**:36-58 全段是 `SELECT f(常量)` 无表查询(读了
   39/46 等原文)——**服务它们只需要表达式求值器,不需要引擎**。
4. **姐妹先例(memory: 写新石头前先查)**:spg-engine 的 eval 家族
   (strings/math/datetime/format/**regexp 3236 行 no_std POSIX-ERE**)
   纯 Rust 零依赖、产线在跑,函数覆盖与清单几乎重合。同属主,
   fork/改写合规;省下的是语义打磨(PG 边角:floor 向 −∞、trim 是
   字符集不是子串、NULL 传播),那正是最贵的部分。
5. **kevy-time 已在位**:civil/epoch/add_months/`@` 表达式(RANGE/EQ
   bound 双面)。日期时间函数(date_trunc/date_part/extract/now)在
   它上面长,不另起炉灶。

## 2. 求值落点(四案)

* **A. 引擎内求值**(查询卡片投影/谓词跑函数)——**违 Law 3,拒**。
  R4a #6(事故 100% 是读聚合)同时说明:把任意函数放进读路径
  正是别人出事故的地方。
* **B. sql 面求值(推荐)**:求值器 = 新石头 `kevy-scalar`;消费者 =
  kevy-sql(`SELECT` 无表 → 常量折叠直接出结果;查询卡片投影列的
  函数 → 卡片带**尾注**(epilogue):引擎按声明访问路径出行,函数在
  客户端(kevy-cli `sql` runner / embedded / FFI 门面)对投影列求值)。
  引擎零改动,Law 3 零风险。
* **C. 写时计算列**(`CREATE INDEX ON t (lower(email))` → 声明期
  编译成派生字段,写路径维护、可索引)——谓词侧函数的正统答案
  (与 ORDERPATH/KIND agg 同族:工作进写路径)。**引擎改动,独立
  拍板件 ②,不挡 V1**:funcgate 语料不需要它;mailrs 实证(app 侧
  预归一化)说明真实负载今天有替代。
* D. 全 app 侧(什么都不做,文档教写法)—— 与「商用 RDS 模块」
  的定位不符,拒。

**推荐 = B 现在做,C 作为 V4/后续拍板件立项。**

## 3. kevy-scalar(石头)

* **形状**:`#![forbid(unsafe_code)]`、零依赖、无 IO;输入输出用
  自己的 `Scalar`(Null/Int/Float/Text/Bool)——不知道 kevy 业务存在,
  semver 可立(石头判据)。
* **面**:`eval(func: &str, args: &[Scalar]) -> Result<Scalar, ScalarError>`
  + 表达式树 `Expr`(字面量/函数/嵌套)+ `fold(expr)`;函数注册表 =
  数据驱动 match(LOC 规则的既定豁免类)。
* **清单**(37 探针反推,PG 语义为准绳,探针文件是验收测试的来源):
  - 字符串:lower/upper/initcap/length/concat/concat_ws/trim/ltrim/
    rtrim/btrim/replace/split_part/repeat/lpad/rpad/strpos/position/
    left/right/reverse/translate/substring/format
  - 数学:floor/ceil(ceiling)/round/trunc/mod/power/sqrt/sign/abs
  - NULL 族:coalesce/nullif/greatest/least
  - 正则:regexp_replace/regexp_match(_es)/substring(text from pat)
    —— **fork spg regexp.rs**(拍板点 ③:fork 还是重写;推荐 fork+
    改壳,3236 行按 500-LOC 规则拆模块)
  - 日期时间:now/current_date/current_timestamp/date_trunc/date_part/
    extract/age + interval 算术(骑 kevy-time)
  - **具名拒**(进边界清单):random(非确定,卡片/复制语义不容)、
    uuid 族(同理;引擎已有别的 ID 面)、to_char 全模板(只做探针
    实际用到的子集,其余按模板具名拒)、timestamptz/timetz(时区
    app 侧,R4a 既定)、md5(可做可不做——探针有,crypto 不引,
    自研 128 行以内可担;倾向做)。
* **NULL 传播**:严格 PG——任意参数 NULL → NULL(coalesce 族除外);
  探针文件里每条都有断言,直接转单测。

## 4. funcgate(操作定义)

* 语料:89 个 `.test` 原文**拷入** `bench/funcgate-corpus/`(工件
  入库,不引用姐妹路径——盒上没有 spg)。sqllogictest 格式解析器
  ~80 行(`query T` / 期望块)。
* runner:`kevy-cli sql eval <stmt>`(新子命令,B 案的常量折叠面)
  逐条喂;有表探针走 `sql plan` 分类。
* 报告:每探针 `SERVED n/m | REFUSED(named) | UNSUPPORTED(silent)`;
  **UNSUPPORTED>0 即 FAIL**(零静默);函数子集 served 比率对 bar。
* 载体:`bench/funcgate.sh`,CI 可跑(纯本地,无盒依赖)。

## 5. 切片(线性)

1. **S1 kevy-scalar 核心**:Scalar/Expr/字符串+数学+NULL 族 + 探针
   反推单测(设计不变量,可即刻开工)。
2. **S2 日期时间**:骑 kevy-time;date_trunc/extract/interval 算术。
3. **S3 regexp**:fork spg 模块改壳(待拍板点 ③,默认 fork)。
4. **S4 sql 面**:`SELECT`(无表)常量折叠 + `sql eval` 子命令 +
   卡片投影尾注(QueryCard 增 `epilogue` 字段,runner 侧求值)。
5. **S5 funcgate**:语料入库 + 解析器 + 报告 + bar 断言 + CI。
6. **S6 文档**:边界清单「函数面进度条」(章程 §2.2 边界诚实行)。

## 6. 拍板点汇总

| # | 事项 | 建议 | 挡什么 |
|---|---|---|---|
| ① | bar 重锚(§0) | B-1 双线 | 挡 funcgate 断言的写法,不挡 S1-S4 |
| ② | 写时计算列(引擎) | 立项到 V4 后议,V1 不做 | 不挡 |
| ③ | regexp:fork spg vs 重写 vs 拒 | fork+改壳 | 挡 S3,不挡其余 |
| ④ | md5 自研与否 | 做(≤150 行) | 不挡 |

S1/S2/S4/S5 在任何拍板结果下形状不变(函数语义是 PG 定的,落点已被
Law 3 钉死在 sql 面)——按 autorun 惯例先行;①③④ 收到拍板后收尾。
