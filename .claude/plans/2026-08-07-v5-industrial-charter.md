# v5 工业版章程(2026-08-07)

> 属主定向(2026-08-07):研究部分已足以支撑,**v5 后续目标转为工业版本
> 支持** —— kevy 本身的提升要做,**RDS 支持作为 v5 的主要附加模块**也要做;
> 重新订立**测试标准、产品能力标准、用户面的产品形态**。
>
> 本章程 = 三套标准的重订稿 + 线性 train 列表。研究弧的全部实测结论是
> 本文的地基(`.claude/plans/2026-08-05-v5-status.md` +
> `2026-08-06-v5-research-plan.md` 二十一期);从此起,**负结果照记,
> 但目标从"探明"换成"交付"**。

---

## 〇、转轨判定(为什么现在可以转)

研究弧交出的、支撑转轨的硬证据:

* **性能**:SME 4-8 核上 idx p99 72-80µs vs PG 164、点查 2.8×、写 25×、
  page p50 与 PG18 打平;R3 攻击面打空。
* **容量**:公式化 `上限 ≈ 值大小/(96B+key heap)`,三档实测吻合
  (256B→2.65× / 1KiB→10.43× / 4KiB→39.2×);对外口径必带值大小。
* **压缩**:corpus 压缩八线门禁全绿,冷读 p99 带内,PG per-datum 结构性
  拿不到。
* **正确性方法**:跨面校验透镜一天挖五个数据丢失级 bug 并全修 + 门禁化;
  "无法被校验的东西必然漂移"从教训变成了机制。
* **定位实证**:R4a 四源证明真实 SME 不用教科书 RDS 核心
  (事务/UNIQUE/FK/SERIAL/窗口函数零出现),事故 100% 读聚合 ——
  **kevy 的形状对着真实需求,不是对着特性清单**。

**已知的未知全部有数**;没有未知的未知挡路。转轨成立。

---

## 一、产品定位与用户面形态

### 1.1 定位一句话

**kevy v5 = 工业级 KV/serving 引擎(Redis 兼容,零依赖单二进制)
+ RDS 支持模块(虚拟 SQL 面 + 迁移工具链)。**
KV 是本体,RDS 是**主要附加模块** —— 模块不是第二个二进制,是文档、
命令面与承诺口径上的一等区隔。

### 1.2 用户拿到什么(发行物)

| 物 | 形态 |
|---|---|
| `kevy` | 单二进制,零外部依赖,单文件配置(默认零配置);内含全部能力(KV + RDS 模块命令面) |
| `kevy-cli` | 运维与**迁移日工具箱**:`sql plan` / `backfill-keys` / `shadow` / `doctor` / `lint`(overlap/columns)+ 既有运维命令 |
| 库 | crates.io 全套 kevy-* + kevy-embedded(嵌入式)+ FFI/napi/wasm 既有渠道 |
| 文档 | 三语,四区重构:**Core KV / RDS 模块 / 运维 / 迁移指南**;边界页(声明式拒绝清单);**容量计算器**(公式 + 值大小三档表) |
| 站点 | kevy.golia.jp 对应四区 + 活 wasm demo(既有) |

### 1.3 模块形态决策

* **不做构建期拆分**:零依赖单二进制是品牌;RDS 模块 = 命令面
  (`TABLE.*`/`IDX.*`/`VIEW.*`/`FEED.*`)+ 文档区 + 独立的能力承诺表。
* 模块可观测:`INFO modules` 报 RDS 面的使用状态(声明的表/索引/视图数、
  AUTODECLARE 状态)——用户一眼知道自己用没用到模块。
* AUTH/TLS 维持既有拍板:**永久不做**,文档以"代理之后部署"为一等路径
  (`deploy-behind-a-proxy.md` 已三语)。

### 1.4 配置与运维承诺(R6 五句话升格为产品承诺)

一个二进制 / 一份配置(默认零配置)/ 备份=拷文件 / 升级=换二进制
(v5.x 内**双向**保证,发布门禁实测)/ 不调优
(唯一具名例外:`--threads ≤ cores−2`,部署形状而非逐负载旋钮)。

---

## 二、产品能力标准(每条都有可测 bar;宣任何数先过 §三的口径纪律)

### 2.1 KV 核心(本体)

| 能力 | bar | 载体 |
|---|---|---|
| 吞吐 | 对标并不低于 valkey 9.1(既有北极星;逐角 perfgate-median 口径) | perfgate-median |
| 尾延迟 | **风暴下无秒级停顿**:任意标准 workload,PING p99.9 ≤ 100ms;reactor 单圈迭代上界 ≤ 100ms(心跳探针机制化进门禁) | 新 tailgate(V3 train) |
| 崩溃契约 | kill -9 任意时点,重启后 AOF 语义完整 | crashgate(既有) |
| 复制 | 快照 ship + 活帧 + 跨代重同步;CDC 帧覆盖全部写动词 | repligate + feed 契约测试(既有,含本弧补的多键/跨分片修复) |
| 容量 | used_memory ≤ 预算×1.05 全程;上限按公式,对外必带值大小 | capacity-envelope + memgate B7 |
| 压缩 | 透明、永不膨胀、冷读 p99 预算内;比率**定性宣称**+按语料类实测附表 | compressgate 八线 |
| 内存(alloc) | 【拍板件 P1】默认 ON:RSS 碎片 ≤ glibc−10%(实测 2.16 vs 2.40)且集合角中位差距按 P1 拍板的 floor 执行 | allocgate + perfgate-median |

### 2.2 RDS 模块(主要附加模块)

| 能力 | bar | 载体 |
|---|---|---|
| 表/索引/视图/窗口表 | 既有面全保;VERIFY **双向 + 按证据对账**零误报 | idxgate/tablegate(既有) |
| **标量函数面** | R4a 清单(~40 标量 + 8 日期时间)实现;**89 探针集 served ≥ 80%**,`sql plan` 输出覆盖率报告 | 新 funcgate(V1 train) |
| 迁移日 | pg_dump→plan→backfill→shadow→doctor **全链演练可重复通过**;引擎接受的每种类型要么被搬走要么被点名 | 新 migrationgate(V2 train) |
| 自动声明 | AUTODECLARE 七属性维持 e2e 钉死;默认关、有预算 | 既有 e2e |
| 聚合/写时冗余 | 事故形状(COUNT/GROUP BY/greatest-n)的 O(1) 答案维持 perfgate 线 | agggate + perfgate |
| 边界诚实 | 声明式拒绝清单(14 条+函数面进度条),每条有门禁或 finding 背书;文档三面孔一致 | check_doc_links + boundarygate 思路(V5) |

### 2.3 对外宣称口径纪律(硬规则)

1. 任何性能数:**perfgate-median(N≥3)口径**,单跑数字不出门;
2. 任何容量数:**必带值大小**;
3. 任何压缩比:定性 + 按语料类附表(同值语料 = 天花板端必须标注);
4. 每个"gated"断言在门禁里真实存在(2026-08-05 逐条核对纪律延续)。

---

## 三、测试标准(七层;每层给"谁在跑、什么时候红")

| 层 | 内容 | 载体 | 红线时机 |
|---|---|---|---|
| L1 单元/属性 | workspace 全套件;fuzz(六石头 + compress 双靶);有效覆盖不灌水 | cargo test / fuzz / covgate | 每 commit |
| L2 跨面契约 | **"在一条路径写、在另一条路径读"透镜制度化**:写路径×读者矩阵(AOF replay / 复制 / 快照 / 索引 / CDC / VERIFY)逐格有测试 | idx/repli/crash/restore-drill | 每 train finish |
| L3 性能 | perfgate-median 为唯一宣角口径;基线 ratchet 只升不降;盒纪律(kevybench/静盒/一次一测) | perfgate-median | 每 train finish + 发布 |
| L4 容量/资源 | envelope 全刻度 + 内存恒等式 EXACT + disk 包络 | capacity-envelope / allocgate-mem / diskgate | 发布门 |
| L5 尾延迟 | PING p99.9 + 迭代上界 + 停顿探针(心跳)常驻 | tailgate(新) | 每 train finish |
| L6 升级/运维 | 双向换二进制 + 备份=拷文件恢复 + 零配置路径 | R6 门禁化(upgradegate,新) | 发布门 |
| L7 迁移(RDS) | 全链演练重复通过 + 工具输出契约(报告器/退出码) | migrationgate(新) | 发布门 |

**CI 政策**:merge 态全 L1-L2 绿才可合;tag 前 L1-L7 全绿
(`gh run watch --exit-status` 唯一姿势);发布 tag = publish 触发器,
不可逆动作前 release-profile 预跑。

---

## 四、线性 train 列表(接 ROADMAP,从上往下)

> 铁律不变:feature 分支先行;【RFC】train 批准前零实现;五轴收口
> (perf/mem/disk/doc/cov)+ 本章程新增 L5-L7 逐步挂门。

* **V0 合并轮** — `fix/idx-drift`(125 笔,数据丢失修复+R4c+文档)先进
  develop;`r1-locality`(38 笔,alloc+compress+rt)随后 rebase 进;
  全门禁 + CI 真绿。**不发版。**
* **V1【RFC】标量函数面** — R4a 清单驱动:求值器(查询卡片投影/谓词侧)
  + `sql plan` 翻译 + funcgate(89 探针集覆盖率报告,bar ≥80%)。
  RDS 模块商用性的最后一块工程。
* **V2 迁移演练门** — 真 PG 库端到端走 R4c 五件,撞墙 finding 化并修;
  演练脚本化为 migrationgate(可重复)。
* **V3 尾延迟工业化** — 心跳探针机制化(常驻可观测 + tailgate 线);
  PING p99.9 bar;慢客户端/连接风暴防护核查。
* **V4 alloc 执行轮** — 按 P1 拍板执行(默认 ON/OFF;若 ON 且要收集合角,
  走残余 RFC 已修正的 B' 信封池化,用 perfgate-median 验收)。
* **V5 产品面** — 文档四区重构 + 边界页(14+函数面进度)+ 容量计算器
  + `INFO modules` + 站点同步。
* **V6 发布轮** — CHANGELOG 收口、upgradegate 双向重测、发布预跑、
  tag(CI 真绿后)、34 crates + npm + 渠道、装后 smoke。

预估:V0 一轮;V1 二至三轮;V2/V3 各一轮;V4 视拍板一至二轮;
V5 一至二轮;V6 一轮。V1 与 V2/V3 可部分并行。

---

## 五、拍板件(P,不挡 V0-V3 开工)

| # | 件 | 选项与我的建议 |
|---|---|---|
| P1 | **kevy-alloc 默认开关** | ON(内存赢 10%,集合角输 10-14%,R4a 证据:SME 事故全在读聚合非集合写)/ OFF(反之)。建议 **ON**,集合角 floor 改按"对上一发布版不退",V4 顺带尝试 B' 收窄 |
| P2 | 集合角 floor 语义 | 沿用 vs glibc-off 0.92 / 改 vs 上一发布 ratchet。建议**改 ratchet**(工业口径应对自己负责,不对假想对照负责) |
| P3 | mailrs 真语料采样 | 涉隐私;建议属主给脱敏采样(仅值形状统计),或明确放弃、以合成语料 + 定性口径出门 |
| P4 | v5 版本时点 | V6 完成即 5.0.0 / 或 V1-V3 后先出 5.0.0-rc。建议 rc 一档,渠道走全流程彩排 |

---

*本章程由 autorun 按属主 2026-08-07 定向起草;三套标准的重订稿即本文
§一/二/三。P1-P4 待拍;V0 可即刻开工(无前置拍板)。*
