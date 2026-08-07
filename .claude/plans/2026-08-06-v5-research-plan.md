# v5 研究计划(2026-08-06,属主授意"安排好做研究,做好计划再 autorun")

> 轴:**从现状到 T9 判定之间、不需要属主拍板就能推进的研究**。
> 三条 track 按 v5 价值排序,线内有序,线间独立。
> 属主项(merge 顺序 / T7 批准 / 补丁版 / alloc 取舍的最终拍板)**不在本计划内**。

## 先修正一个我上一轮报告里的过期结论

我在 v5 进展报告里说 alloc 的 residual "设计轮待拍板" —— **那是 v8 收口时的状态,
已经过期**。`r1-locality` 在 v8 之后又走了三步(读实,非记忆):

1. `21401eaa` **per-word bit batching 已实现** —— 远线触碰 64:1 摊销,位级
   position-awareness 粗化到词级而不是被绕过;
2. `c023ff8a` **12 角 A/B 实测**:三大集合写角收敛(compat_set +5.4pp 转绿、
   lpush +6.8pp 转绿、hset +6.1pp),sadd/zadd 平(长寿命节点错过 claimed word),
   **M3 分毫不动 1.98× vs glibc 2.40×** —— 正是杀死 LIFO cache 的那条轴;
3. `62797c6b` **hot-slot 层已实现、未验证** —— 32 槽 LIFO,只持当前 span 的
   刚释放槽(空间界封住 LIFO cache 的旧伤),服务 per-word claim 够不到的那一层
   (hset 实测 3.06× L1 税所在的旧对象替换形状)。**commit 自己写着:
   "Next: the two-axis retest on lx64."**

⇒ 所以 R1 不是"开设计轮",是**执行那条 train 自己写明的下一步**。

## R1 — hot-slot 层的 lx64 两轴复测(alloc train 自己的下一步;v5 判据②的关键)

- **做什么**:在 lx64(kevybench 账号,perfgate 硬拒 root)fetch `origin/r1-locality`,
  build,复跑 `c023ff8a` 同款 12 角 interleaved perfgate + M3 手工探针,并读
  hot-slot 的 hit/full 计数器(RFC 的判据门)。
- **判据**(train 自己定的,不新造):① sadd/zadd/hset 残余角是否向绿收敛
  ② M3 必须钉在 1.98×(任何回吐 = hot-slot 重蹈 LIFO cache) ③ hit/full 计数
  说明机制真的在被打中。
- **产出**:finding doc,**用本地 worktree 提交到 `r1-locality`**(那条 train 的
  记录归那条 train;不 push —— push 归属主;worktree 用完即清,残留教训在案)。
- **已核实的前置**:`ssh lx64` 通、box 空闲(load 0.05)、checkout 在
  `/home/kevybench/kevy`(root 直读会 dubious-ownership,必须 `sudo -u kevybench`)。
- **注意**:`c023ff8a` 自己记着 "allocgate-mem's runner owes a sequencing fix"
  —— M3 数字用手工探针取,同款 workload,不信 runner。

## R2 — compress K4 前提的杀/证实验(T3/T4 开工前的先验;本轮方法论的直接应用)

- **为什么先做它**:`r1-locality` 上的 `compressgate.sh` 自己写着 —— "K4 是判定
  这个设计值不值得做的那一行;**若 K4 无法通过,那就是 finding,它退役这条 train**,
  而不是触发另一轮调参"。R4c 五件里三件的前提在开工时被推翻 —— 这条 train
  还没花一行实现,先验前提最便宜。
- **做什么**:不写任何产品代码。构造有代表性的值语料(N 个相似 400B 值 /
  真实 JSON 行形状 / 对抗性不可压语料),用参考实现作 oracle(纯研究仪器,
  不是依赖 —— 零依赖铁律约束的是产品)对比 **per-datum 压缩 vs 共享字典压缩**
  的比率差,量出 K4 主张的"跨值冗余"到底有多少真实存在。
- **产出**:finding doc。前提活 → T3 开工时带着实测语料基线;前提死 →
  按 compressgate 自己的话退役 T3/T4,T9 的判定条件相应改写(负结果是产出)。

## R3 — 写路径透镜扫尾:剩下的派生结构(本分支;第 12 个缺陷的最可能藏身处)

- **已扫**:range(抓到 TTL 过期缺陷)、agg(VERIFY 证伪不了,网=测试)、
  unique(duplicates 按分片,文档已改口)。
- **未扫**:`KIND text`(倒排+BM25 统计)、`KIND ann`(HNSW 图)、
  `MODE materialized` 视图(TOPK 物化片)、窗口冷段(过期 × 冷行交互)。
  每一个都是"另一条路径写、这条路径读"的派生结构,与已抓十一个缺陷同族。
- **做什么**:把 `index_write_path_coverage` 的逐动词-即时对账模式推到这四个
  结构上;拒绝静默跳过(COPY 教训);每个结构先读它的 VERIFY 面,确认
  "网在哪"(text/ann 的 VERIFY 能证伪什么、不能证伪什么,与 agg 同题)。
- **产出**:测试 + 可能的缺陷修复 + 文档改口(如有高估的保证)。

## 顺序与中止条件

**R1 → R2 → R3**。R1 若被 lx64 环境挡住(box 被占 / 构建不过),不空等 ——
记下阻塞点,降级去做 R2/R3,回头再试。任何一条 track 发现"前提错了",
按 arc 铁律⑤:改前提、留档,不绕过去。

每轮收尾纪律(hygiene.md 全套):本地 rootgate、lx64 清点(`ls ~ | grep -E "^(aof-|dump-)"`
应为 0、/tmp/kevy-* 空、不留新 checkout)、worktree 用完即清、零等待循环。

---

## 执行结果(2026-08-06 当日回填)

**R1 ✅ 两轴复测完成,双轴皆负 —— hot-slot 层是干净的 REVERT。**
吞吐:三个目标角比 per-word 轮各恶化 4-5pp(hset −17.0 / sadd −15.7 /
zadd −18.4),lpush 差 0.5% 回红;M3:1.98× → 2.067×(把对 glibc 2.40×
优势的四分之一还了回去)。探针自验:OFF 侧复现账上的 2.40× 到分位、
两侧 used_memory 到分位相等。失败机理 = LIFO cache 的微缩重演(缓存槽
从 span 视角仍是活的,钉住了致密化本可归还的页;空间界把伤口压到 0.09×,
但方向是错的,而且吞吐是负的)。finding 已按 train 纪律落在
**`r1-locality` 本地提交 `b1bfdcfe`**(不 push;revert 与否归属主/那条线)。
附带记录:判据门的 hit/full 计数器**没有 envelope 级载体**(HotStats 只有
单测在读),下一个设计想区分"冷机制 vs 有害机制"得先给它出口。
**仪器学费两笔**:perfgate 的 preflight 会匹配启动链 argv 里的
`/home/kevybench/...` 路径(两次假拒都是门照见自己的调用者 —— 路径必须走
脚本文件体,永不上启动 argv);allocgate-mem 的 runner 在报数前退出
(已知时序债,手工探针同参数取数)。

**R1 追加(同日):仪器债一还一记。** ① `allocgate-mem` 的 "sequencing" 死因
已定性并修复 = **接口漂移**(`info` 长出必填 `--field` 且改印裸值,runner 仍
裸调+sed 旧前缀;就绪循环同病所以烧满超时假装就绪,取数行在 pipefail 下
杀死脚本于 run_one 中途 —— **cleanup 之前**,泄出的 7415 服务借 SO_REUSEPORT
与下一轮并存分流,制造出 used 939MB > 预算、RSS < used 的谎数)。修复已
端到端验证(OFF 2.40× / ON 2.06-2.09×,与手工探针吻合),落 `r1-locality`
本地 `105eecc3`。**教训入账:死在 cleanup 之前的仪器不只失手一次,还会武装
下一次测量去说谎。** ② **具名未修**:`allocgate.sh` 的 M2 spread 计算有
重定向 bug(把比值写成文件名,repo 根留 4 个裸数字文件,spread 显示为空;
判定数学不受影响)—— 留给那条 train,不静默。

**R2 ✅ K4 前提活,带三个设计输入。** `bench/FINDING-2026-08-06-k4-premise-…`:
identical 语料上字典模式字面就是 O(字典)+N×9B 而 per-datum 永付 89B/值;
真实 JSON 行 per-datum 留 44% 在桌上;随机语料 per-datum **膨胀**(411>400,
K2 早退是实测需求);**字典构造(非 match-finding)是捕获的胜负手**
(32KiB 采样字典在真实行上拿到天花板的 72%,在 identical 上只拿到 6%)。
工具入库 `bench/k4_premise.py`。

**R3 ✅ 全部扫完。** text × 过期:行为过硬(doc 退出 MATCH、BM25 重打分);
顺手抓到**第 12 个发现并已修**:非标量 kind 的 IDX.VERIFY 四元组被按位置
贴上标量审计标签(健康 text 索引答 `coerce_failures 7, duplicates 7`,
agg 的 groups 印成 duplicates,ann 把 links+rebuild 两事实挤一数)——
已改为 kind 词表作答(chunk 带 tag 字节;进程内扇出,标量逐字节不变)。
**补扫结果(同日)**:ann × 过期 ✓(向量退出 KNN,墓碑+rebuild 建议正确,
新词表实测在线);materialized 视图 × 过期 ✓(TOPK 正确补位);
窗口冷段 × 过期 ✓(已封冷段中的行过期后从跨窗 RANGE 消失 —— 缺陷 #5 的
墓碑机械与过期漏斗修复正确复合)。**四个结构零新缺陷**,过期漏斗
(`note_expired` → `drain_expired_keys` → `note_key_mutated`)对所有派生
结构一次覆盖 —— 修在漏斗上的价值在此实证。

---

## 二期(2026-08-06 晚,属主点名"继续做 v5 研究"后追加)

> **先认一笔方向漂移**:R1-R3 收口后我去填了 electron/tauri 翻译 ——
> 那是 i18n 门禁的具名债,但**不在 v5 轴上**。已被属主点名纠正。
> 二期回到 T9 判定条件倒排的真缺口,全部自主可做。

**R4 — alloc 残余税的 Phase-A decomposition(v5 判定条件②的正面)。**
per-word finding 自己写明的下一步:"profile whether the remaining hset tax
is still allocator self-time or has moved (Pre-Phase-B gate before anything
is built)"。hot-slot(机制猜测式的一轮 polish)已被两轴否决 —— 按 perf 方法论
这正是"停止猜机制、切 decomposition"的触发点。
做法:lx64 checkout **c023ff8a**(per-word 态,不带将被 revert 的 hot-slot),
双构建,`perf record`(root 跑 perf、kevybench 跑服务 —— paranoid=3 的学费在案)
分别压 hset 与 zadd 角,对照 v8 账的 17.3% vs 10.1% 基线回答:
① 分配器 self-time 是否已被 per-word 压下去(→ 税移到哪了,分解新去处)
② 还是仍驻分配器(→ "allocation-site grouping 能否让 sadd/zadd 的 free
word-local"才是活候选) ③ 或者按 finding 的第三种可能,税属于
**value-representation 单元而非分配器**(→ 改前提,不是改分配器)。
产出:decomp doc 落 `r1-locality`。

**R5 — pubsub 残余(0.83-0.86)的同款 decomposition。** v8 账只说了
"different residual class",从未具名。同一个 lx64 profile 会话顺带压一轮
pubsub,给这个 class 起名字。M2 地板 0.92 是 v5 判定条件②的另一半。

**R6 — K1 解码预算的量化(compress 的下一个前提,K4 的续集)。**
K1 断言"解码必须 memcpy 级"(spg lzss 100MiB/s 会吃掉 105µs 冷读预算里的
40µs)。在 lx64 实测:memcpy 4KB 的真实 µs、一个朴素 LZ 解码环的真实吞吐、
预算里可花的解码份额 —— 把 T3 最硬的约束从断言变成数字,与 K4 同款
(纯研究仪器,零产品代码)。

顺序:R4 → R5(同一 profile 会话)→ R6。产出全部按 train 归属落盘。

### 二期执行结果(同日回填)

**R4 ✅ decomposition 完成,答案比预设三分支更硬** —— finding 落
`r1-locality` 本地 `bdf88ae0`(`PERF-DECOMP-2026-08-06-collection-write-residual.md`):
① 具名 alloc 路径只比 glibc 贵 ~3pp(13.5-14.0% vs 10.4-11.0%),解释不了
−12~−17%;② 更大的 +9-12pp 藏在 tick 邻接符号区间(fat LTO 不给名字;
唯一 ON-only 的 tick 组件 = `thread_reclaim()`,即 M3 自己的机制);
③ **用"关掉 reclaim 来定价"的实验失败得恰好回答了问题**:NR build 在
perfgate 累计 ~300M 写后憋死(两次复现,脏/净 checkout 各一),而新起服务
60 秒 zadd 全稳 —— **reclaim tick 是活性承重,不是可选成本**;
④ 1.8 秒突发三方全平(~4.56M)—— 税是稳态现象,短跑仪器永远看不见。
**⇒ 下一设计候选 = reclaim 节流(每 tick 有界预算 / 每 N tick 摊销),
优先于任何进一步的 alloc 路径打磨。** 四笔仪器学费全部入档
(pgrep 选中 sudo 包装 / strip=true 吃掉 debuginfo / `$(…|tail -1)` 吞
benchmark 输出 / 盒上 checkout 根 289 个 aof 残留)。

**R5 ✅ 残余 class 已具名,且与 R4 合流** —— finding 落 `r1-locality` 本地
`bbd83b16`(`PERF-FINDING-2026-08-06-pubsub-residual-is-page-refault.md`):
`clear_page_erms`(内核零页填充)在 ON 下近乎翻倍(12.2% → **21.3%,跃居
第一符号**)。机制 = kevy-alloc 把页还给 OS、下一突发 refault 回来付零填;
glibc 从不还页所以从不付。**与 R4 是同一个机制的两张脸**(reclaim 的用户态
tick 成本 + 内核侧零页税)⇒ **设计候选合流为一个:还页滞后/节流**(挂在
记账既有的 hysteresis 项下,按 M3 包络定尺寸)。判定所需仪器全部在位。
**R6 ✅ K1 预算已量化** —— `bench/FINDING-2026-08-06-k1-decode-budget.md`
(+ 探针 `bench/k1_budget.rs` 入库):目标盒上 memcpy = **0.183µs/4KiB**,
词级 wildcopy 解码环 = **0.486µs**(memcpy 的 2.7×,预算的 0.46%);
**要求量化为"解码 ≥ ~1 GB/s"**(4µs = 预算 3.8%),RFC 的 token-nibble +
wildcopy 草图裸写都超一个数量级;100 MiB/s 那一档真出局(38%)。
一行坏仪器如实弃用(byte-loop 被向量化成平凡填充,比 memcpy 还快 ——
它不再是解码器的证据)。**K4(捕获在字典构造)+ R6(解码有 ~20× 余量):
T3 最硬的两个约束现在都是数字。**

### 研究计划全线闭合(2026-08-06)
R1-R6 六条 track 全部收口,**外加设计轮 RFC 已出待拍**:
`r1-locality` 本地 `c837f9ac` = `.claude/rfcs/2026-08-06-v5-reclaim-pacing.md`
—— 一个旋钮打两个残余(设计主张:两笔税定价的是页**何时**归还而非能否;
M3 要的从来是有界不是激进,记账的 hysteresis 项本来就是那个载体)。
四个候选形状(倾向 A 衰减门,可与 B 每 tick 预算复合;倾向持为待杀假设),
一条不可让步的活性界(每个空闲页在有界 tick 数内必归还 —— NR 憋死机理
未确认,界不能依赖知道它),验收全部由已在位的仪器承载,四个拍板点给属主
(含"接受 SME 取舍不做了"这一选项)。
**v5 轴自主面至此真正清空;剩余全部归属主**(本 RFC 批不批 / hot-slot
revert / merge 顺序 / T7 / T3 开工位置 / push 与补丁版)。

---

## 三期(2026-08-06 深夜,属主定向"v5 工业化"):pacing 两轮实施

**属主授权后动了 `r1-locality` 的代码**(bundle 送盒,不 push):
revert hot-slot(`947a4660`)→ 候选 A 实现(`db02e8c8`)→ **统一门被 M3
一行否决**(2.40× = 优势归零;B6 量的是**搅拌中的 RSS 峰**,差分门在持续
搅拌下永不放行 —— **M3 的 1.98× 不是空闲态地板,是边搅拌边归还**,RFC §2
的设计主张对 span 域是错的;zadd 憋死还复现)→ **按域拆分**(`2564e2f5`:
span 恢复激进,池留代龄门)→ 电池:

| 线 | 激进(晨) | 统一门 | **拆分** | 目标 |
|---|---|---|---|---|
| 活性 | ✔ | zadd 憋死 | **✔** | ✔ |
| lpush | 红 −8.5 | — | **绿 −6.0** | ≥0.92 |
| hset/zadd | −17.0/−18.4 | — | **−13.1/−14.3** | ≥0.92 |
| sadd | −15.7 | — | −17.1 | ≥0.92 |
| pubsub | 0.83-0.84 | 0.841 | **0.881** | ≥0.92 |
| M3 | **1.98×** | 2.40× | **2.16×** | ≤1.98× |

**又一个被数据纠正的断言**:"B6 摸不到池"错 —— demote 搅拌期有大映射走池,
64 代滞留抬 RSS 峰 ~80MB(M3 付 0.18×)。池确是 pubsub 零页税所在
(+4-5pp 验证),但与 M3 形状**不相交是假的**。
finding = `cfa68922`(round-2,含下一步的 decomposition 序:① 给池的
B6 房客起名(demote 批缓冲嫌疑;池内按域再拆是结构解,调 AGE 是 polish 解)
② 拆分版下重测 clear_page 份额 ③ sadd 逆行单查)。
**RFC 判据未达成;按方法论这是 re-decomp 前的最后一轮假设。**
`r1-locality` 本地 +10 笔,未 push。

---

## 四期(2026-08-07,pacing round 3 —— 按 round-2 finding 的 decomposition 序执行)

**三步全跑完,三个自我修正全有同会话对照钉着:**

1. **池的 B6 房客点名**(临时探针:park/take/reject 直方图挂 reclaim tick):
   整个 B6 **只有 6 次 park** —— 一个 64MiB 通用启动映射(pubsub 服务器里
   也一样出现)+ 36K-588K ladder 各一笔(~1.1MB)。"demote 批高频搅拌"
   证伪:池在 B6 近乎静止。
2. **M3 归因二次翻案**:caps 把 64MB 和 ladder 全赶出池后 RSS 一字不差
   (2.16×)。三方同会话实验(glibc / eager-947a4660 / caps)裁决:
   **eager 今天也是 2.16-2.17×,glibc 2.40×** —— 晨间 1.98× 是跨会话盒
   漂移,round-2 记的"池滞留让 M3 付 0.18×"**本身是错的**(方法论 §1
   "single run" 反模式的跨会话变体)。**pacing 各轮对 M3 全部零代价**;
   M3 判据诚实重锚 = 与同会话 eager 打平(已达成)。
3. **zadd "憋死"洗清**:隔离复现 2×2(round-2 版 / caps 版)——各 1 次
   >3s INFO 停顿、**全部完赛 ~3.6M/s**。与构建无关的既有尾缺陷
   (疑点:巨型结构增长 alloc+copy+zero vs glibc realloc 的 mremap);
   round-1/3 的 REFUSED 与 round-2 的绿只是硬币两面。
   **pacing 从头到尾没碰过活性。**
4. **M2 机制看清**:持续负载 profile(第一次采样踩了"200k msgs <1s 跑完、
   perf 采空转服务器"的坑,重跑 20M msgs)—— split 下 ON clear_page
   9.3% ≈ OFF 10.2%,**refault 税已闭合**;剩余 4pp 缺口在
   `deliver_publish` 自身(+6pp,header-free cache line 成本),
   **不是 WHEN 轴能买的**。稳态 pubsub 根本不碰池;round-2 的
   0.881 增益来自**跨 bench 实例的连接拆除缓冲复用**;按长度限占 v1
   把它掐掉(0.863),撤销。
5. **sadd 与 hset 同源**:allocator 自身符号 ON 13.1pp ≈ glibc 11.4pp,
   +7pp 在 `drain_replica_inbox`(tick 邻接 LTO 区间,与 R4 结论合流);
   −17.1 vs −15.7 是盒漂移,非独立机制。

**实现走了三版,前两版各被一行数据否决**:
v1(按长度限占 4)→ M2 0.863(掐掉跨实例拆除缓冲复用,撤);
v2(128 槽)→ M2 收复 0.879,**但 B6 一腿 RSS 883MB / 2.59× 比 glibc
还差** —— 槽位富余让**不同长度的死条目**(exact-length 永远配不上;压实
类变长缓冲嫌疑)在 64 代 aging 到期前囤成无界尸堆,32 槽本来就是结构性
兜底。**v3 终版 = 32 槽 + `POOL_MAX_LEN=1MiB` + aging、无按长度限占**,
每个数字都有同会话实测背书;单测钉"整潮进池/下一波取走/巨型必拒"
(第一版手搓 park 破坏恒等式被并发测试逮住,改走公共 alloc/dealloc
生命周期)。
**仪器债清偿**:allocgate M2 spread 的 awk `printf …, NR>1 ? …` 裸 `>`
被解析成重定向 —— 比值命名杂散文件 + spread 空串,一个根因两个症状,已修
(round-2 OFF spread 实测 1.4%,M2 的 0.88 远在噪声带外)。

**v3 终版电池**(perfgate 完整跑完,zadd 硬币正面):活性 ✔ 全 12 角;
M3 **4/4 腿 2.16-2.17× 全稳**(平价判据达成);M2 0.832;集合角
sadd −15.8 / hset −12.0 / zadd −14.4(盒漂移 ≤2.9%);lpush/KV 全绿。
**第四个修正来自表本身**:历轮 M2 = 0.83-0.88(eager 0.83-0.84 / split
0.881 / v1 0.863 / v2 0.879 / v3 0.832)—— **±0.05 跨会话噪声带吞掉一切
池效应主张**,round-2 的"aging +4-5pp"不过自家方法论;可靠的是 profile
对(零填平价 + deliver_publish +6pp)。v3 以**结构理由**入库(滞留双界 +
全轴零实测代价),不以吞吐主张入库。

**设计收束**:M3 与活性两条线**已了结**(pacing 零代价);M2 剩余缺口
(delivery layout)与集合写地板(tick 税)都**不在 WHEN 轴上** ——
pacing 弧到此闭合,下一轮按方法论 = 对 tick 税的全新 decomposition
(zadd >3s 停顿与 mremap 优势是两个具名入口)。

---

## 五期(2026-08-07,tick 税 decomposition —— R4 归因翻案,真凶定量闭账)

**三个具名嫌疑全被计数器杀掉**(sadd 60M/11s 全场探针:sweep 七计数 +
大块 realloc 拷贝 + caller 计时):sweep 769 次共 **55.5ms = 0.06%**
(tick=100ms/hz=10,R4 的"税在 thread_reclaim"被直接测量翻案);
discard→refault 671MB ≈ 0.6%;大块 realloc 拷贝 2.74GB ≈ 1% **且与税
不相关**(zadd 拷贝只有 1/38,税同级)。

**真凶 = headerless 元数据步的 L1 misses,预算全额闭合**(perf stat
同窗 A/B):ON 每 op 指令**少 2.4%**、branch miss 好 3.6×,但
L1d miss **27.2→75.8/op(×2.78)**,IPC 1.67→1.48;+48.6 miss ×
~13 cyc ≈ +630 est vs **+613 实测,对账 ~3%**。停顿落在执行中的消费者
符号上 —— 各 profile 的 deliver_publish +6pp / drain_replica_inbox
+7pp 全是同一机制('2026-07-26-header-free-costs-a-cache-line' 的
定量收束)。集合角税重 = 每 op alloc/free 多;KV 绿 = 少。

**开放**:zadd >3s 停顿(已排除 sweep 与单次 realloc;剩内核嫌疑)/
mremap(2.7GB 可免拷贝,吞吐 ≤1%,尾延迟候选)。
**攻击面已具名**(下一设计轮 Phase B):热元数据并线(bitmap+claim
word 同 cache line)/ span header 预取 / 位图批写 / 或有代价地把
free-list 词放回被释块。finding =
`bench/PERF-DECOMP-2026-08-07-collection-tax-is-l1-misses.md`
(r1-locality `7e944a63`,现 +14 笔)。

---

## 六期(2026-08-07,Phase B 第一刀 —— inbox 旗门 + 干预检验)

**miss 事件采样(不是 cycles)把 27.25% 的全部 L1 miss 定位到
`drain_replica_inbox` 区间内一条内联原子**;源读揭底:server 给每个
shard 无条件建 replica inbox,reactor 每迭代付"1 次 Vec(64) 分配 +
1 次空 mpsc 探测 + 1 次共享 line store",8 个 shard 的信号原子被密排
进共享 cache line 互相 ping-pong(glibc 靠 chunk header 恰好垫开 ——
布局运气,不是设计)。

**修 = 旗门早退**(wake 契约本保证发送必举旗;打满 1024 自己重举旗)
+ `InboxSignal` `#[repr(align(64))]`(`a63cf47d`)。判决:
- 机制:miss 3.31B→1.59B/8s(OFF 1.30B),风暴外科级消灭;
- 契约:repligate 全 PASS(快照/活帧/重启/SIGKILL 跨代)+ 205 套件绿;
- 吞吐:**sadd −15.8→−7.8 转绿**;hset −13.4 / zadd −15.2 不动;
  incr 单跑 −11.5 经 3 轮插值洗清为噪声(+0.8% ±3%)。

**方法论修正(重要)**:串行预算"48.6 miss × 13cyc = 全部缺口"是
数字巧合 —— 消灭 1.7B miss 只回收一个角:被轮询 line 的 miss 大半被
乱序引擎藏掉。**纸面对账 = 与因果相容,不是因果证明;干预才是检验**
(与 v1.29 "memcpy 是税不是瓶颈"同构)。已入 finding(`5c7181c1`)。

**剩余集合税的形状**:hset/zadd/incr miss 画像全平(顶行 4-6%),
指令/op 比 glibc 少、miss 近平价、IPC 仍 1.50 vs 1.67 ——
**下一轮开局 = topdown stall 分解**(store/RFO、依赖链、前端),
不再猎 load-miss。r1-locality 现 **+16 笔**。

---

## 七期(2026-08-07,hset 税四层剥洋葱 —— 瓶颈线程付快路径)

四层测量各杀上一层假设:① TopdownL1/L2 进程级 ON/OFF 几乎全同
(retiring 48.8/50.0、memory-bound 5.0/4.9)—— 不是 stall 故事(进程级)
② 指令形状漂移:批处理→每迭代开销;直接数 syscall:**OFF 3.85-3.88
enters/op,ON 4.47-5.15(+15-34%)** ③ per-thread 拆穿拓扑:hset 单热键
→ **owner shard 8s 只打 2.6k enters(饱和、从不阻塞、就是吞吐上限),
7 个转发 shard 各打 ~20M 空转** —— 此前所有全进程 profile 都被 1:8
稀释(包括本弧"allocator 自身符号平价"的读数);转发者多打的 enter
是症状不是因 ④ **只采 owner 线程:kevy-alloc 快路径 23.3%
(alloc 10.5 + dealloc 9.2 + pop_slot 3.6)vs glibc 13.4%
(malloc 5.0 + cfree 8.4)—— 每调用成本 ~1.7×,+10pp 瓶颈线程
≈ −12% hset 税**。无风暴、无停顿、无 tick —— 快路径每调用就是比
tcache push/pop 干得多。

**下轮攻击面(探针先行)**:dealloc claims-first 重排(回收命中路径
根本不需要读段头 owner)/ claims 命中率计数(alloc 2× malloc 提示
refill/慢路径超设计预期;已删 LIFO 缓存的 M3 教训框住设计空间)/
zadd 用一次 owner 线程采样验证同形。

**纪律学费**:①饱和单 shard 角 = 单线程测量,全进程 profile 稀释 1:8,
要采瓶颈线程 ②本轮曾两个 bench 同盒重叠(违反隔离纪律),污染轮
弃用,wait-for-quiet 守卫已进脚本。finding =
`bench/PERF-DECOMP-2026-08-07-hset-tax-owner-thread.md`(`ee98f000`,
r1-locality **+17 笔**)。

---

## 八期(2026-08-07,快路径轮 —— 命中率、重排落地、平坦残差)

**分支率探针**:claims 命中 alloc 99.88% / free recycle 99.86%,慢路径
与段扫痕量 —— 攻击面 = 命中路径每调用成本本身(探针自身全局原子污染了
一次 owner profile,比率有效、画像弃用 —— 又一笔仪器学费)。
**claims-first free 落地**(`42198079`):recycle 路径不再读段头
(匹配即所有权证明);hset owner 3 轮插值 **+0.8/+2.4/+4.4%,均值
+2.5%**;增行触发 500-LOC 门 → free 侧拆 `heap_free.rs`(手术第一版
脚本切坏括号,人工重做 —— 结构拆分不该用正则)。
**重排后 owner srcline 平化**:顶行仅 3.94% = `segment.rs:141` 的
`off / size_of(class)` 整除(每 free 一次)—— **最后一把具名刀 =
按类倒数魔数表(mimalloc 同款),值 ~2-4%**;`class::index_of` 已是
查表,其 3.92% 是入口摊派不是刀。
**诚实残差**:门(+8pp sadd)+ 重排(+2.5% hset)之后,ON vs glibc
集合差距是**摊薄在整个 alloc/free 机器上的 ~10%,无主导座位** ——
再收要么许多小刀(倒数除法先行),要么 class 形状 / claims 宽度
重设计(与 M3 互动,**设计决策归属主,不是 autorun 尺寸的改动**)。
r1-locality 现 **+19 笔**(tip `ddec7b2e`)。
