# R4b:自动声明闭环 —— 拒绝即需求信号,决定在声明期

> 总案 C2 已授权的研究单元。判据实验形态(总案原文):"从未见过的
> 业务负载,零人工声明,N 分钟后全部查询有访问路径"。
> Law 3 不破:观察 → **一次性声明动作**,永不 per-query 规划。

## 〇、信号面盘点(现状即富矿)

1. **运行期拒绝自带全部参数**:`IDX.QUERY <table>.<col> …` 打未声明
   路径 → ST_NOINDEX。名字约定给出 (table, col);verb+shape 给出
   kind(RANGE/EQ → range;WHERE 分量列序 → ORDERPATH;MATCH →
   text);FILTER 字段列表给出 VALUES 声明。**拒绝请求 = 完整的
   声明规格**,无需任何推断。
2. **kevy-sql 编译期已有同构报告**(viewplan.rs:"WHERE (…) matches
   no declared access path — add: CREATE INDEX ON t (…)")——
   advise 输出直接复用它的文案形。
3. 表 catalog 已有列类型 → 新列索引的 TYPE 可查;FILTER 的
   clause_error 已列 "it stores: …" —— VALUES 缺口同理。

## 一、三面拍板

### 观察面(engine,常驻零成本)
- process-global 有界观察表(catalog 侧,LRU 128 项):
  key = (name, shape-hash),value = { count, first_seen 样本 argv,
  推导出的声明规格 }。**只在拒绝路径写**(ST_NOINDEX / WHERE 无
  路径 / FILTER unknown-field)—— 命中查询零触碰。
- 使用面对偶:每 declared 路径挂 hit 计数(query 路径 +1,原子
  relaxed)—— 回收面的原料。

### 批准面(两档,默认保守)
- **advise(默认)**:新命令 `IDX.ADVISE` 输出建议清单 —— 每项
  = 可直接执行的声明命令 + 观察计数 + 样本;零使用路径的 drop
  建议同列。人拍板,engine 只陈述。
- **auto(显式授权)**:`TABLE.DECLARE … AUTODECLARE <n>` 旗标
  (每表上限 n 条 auto 路径)。观察计数过阈(如 ≥16 次拒绝)→
  engine 在**声明期动作**里补 declare(走既有 IDX.CREATE 全路径:
  backfill/-INDEXBUILDING/内存账)。上限保护内存;auto 建的路径
  打标(IDX.LIST 可见 auto=true)。
- **删除永远归人**:建是加法安全(最坏浪费内存且有界),删是
  减法危险 —— auto 只建不删,零使用只进 advise。

### 回收面
- hit 计数 + 最后命中时间戳进 IDX.LIST;`IDX.ADVISE` 报
  "declared but unused for …" 清单。窗口收窄建议同此面:观察到
  某窗口列 bound 分布全落近端(@now-δ 内)→ advise
  "WINDOW … SPAN 可收窄至 δ"(与 R2 的合成点,只 advise 不 auto)。

## 二、切分(实施轮,各自 RFC 不再另立)

1. **R4b-a 观察 + IDX.ADVISE**(纯加法,默认行为零变化):拒绝路径
   挂钩 + 观察表 + hit 计数 + ADVISE 命令 + e2e(打零声明表 N 形
   查询 → ADVISE 输出恰为所需声明集,执行后全命中)。
2. **R4b-b AUTODECLARE**:旗标 + 阈值 + 声明期动作 + 上限/打标 +
   判据实验(零人工声明 → N 分钟全命中)。
3. R4b-c 窗口收窄 advise(R2 合成)—— bound 分布采样,独立小轮。

## 三、发散区

- 阈值/上限数字(16 次、128 项、每表 n)全部实测调,不预辩。
- embedded 面镜像(观察表在 Store 级)—— a 轮同做或紧随。
- kevy-sql 与 ADVISE 的互通(ADVISE 输出 SQL 形?)—— R4c 工具链
  的接口,记档。
