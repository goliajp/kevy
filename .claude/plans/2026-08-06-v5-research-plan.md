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

---

## 九期(2026-08-07,倒数魔数刀 + 官方双刀角表)

`slot_index_of` 整除 → 按类 `ceil(2^32/size)` 乘移位(`0ce0397d`;
Granlund-Montgomery 域内精确 + **79 类 × 64Ki 偏移穷举测试** ——
注释里的证明没有 CI)。hset owner 6 轮插值(剔 1 条 base 塌陷离群):
**+3.9% 均值,轮间方差显著收紧**。

**官方 perfgate 双刀角表**(完整跑完,盒漂移 ≤±4.3%):
sadd **−15.8→−8.5(距绿 0.5%)** / hset −13.4→−11.6 / zadd
−14.4→−11.7 / **lpush −4.3 转绿** / incr/get/set 绿 / zinterstore
+18.9(微 n 角,软看待)。**教训:ad-hoc 刀均值(+2.5/+3.9)对
ratchet 兑现打折 —— 带边均值 + 轮间散布会夸大,官方插值表才是账本。**

弧总账:集合角全线双位数→个位数;残余 = 平坦 ~10% 机器摊薄
(class 形状/claims 宽度设计题,M3 互动,归属主)+ sadd 差半个点。
r1-locality 现 **+21 笔**(tip `68ec04b1`)。

---

## 十期(2026-08-07,残余 RFC + 角表噪声带曝光)

**残余设计 RFC 已出**(`.claude/rfcs/2026-08-07-v5-fastpath-residue.md`,
`23536fc1`):三扇门 —— A 接受(headline 是 KV+查询+内存,R4a 证
prod 事故 100% 读聚合)/ B claims 加宽(双词或 128 槽,半 refill 频率,
保 lowest-first,估 1-3%,需含 M3 四腿电池)/ C class 形状重设计
(RFC 级,动核心论题,无目标负载不开)。**推荐 A-then-B**;另设更上位
拍板点:集合角 0.92 floor 是否仍应是 v5 门(floor 定立早于税形状被测清)。

**perfgate 复跑曝光角表噪声带**(`ab4e9bf5`):同 tip 两跑 ——
sadd −8.5/−9.6、hset −11.6/−9.4、zadd −11.7/−15.7、set −7.4/−10.5、
incr −5.5/−11.5,ref 腿自身 +3-5% 漂移。**每跑带 ±3-6pp;
"sadd 差 0.5% 到绿"是单跑幻觉;宣绿/宣红需 median-of-N**(方法论
bench-infra 条款)。诚实弧终账:集合角全部从双位数负改善到个位数负带,
**除 lpush 外无一可证绿**。r1-locality 现 **+23 笔**(tip `ab4e9bf5`)。

---

## 十一期(2026-08-07,zadd >3s 停顿关闭 —— 五个死嫌疑与一个无界循环)

五连消(每个都被测量杀死,无一靠论证):巨型 realloc(计时探针 10 次
停顿零命中)/ 内核阻塞(现场内核栈全 R 无 wchan)/ SYN 丢弃
(ListenDrops=0)/ 错过唤醒(第二快照 8 反应堆全 R)/ SQE 延迟提交
(每圈都 submit,owner ~3ms 一圈)。**心跳探针一击定位:shard 4
(热键 owner)单圈迭代冲 1.6s、其余 shard ≤50ms** —— `drain_inbound`
的 `while pop()` 被 7 个转发者边排边填喂到永不退出,owner **自己的
直连客户**(accept/新连接 recv)全程挨饿;"停顿"期间吞吐从未下降
(转发者走环)—— 症状层错了三次的原因。

**修**(`d5312749`):`DRAIN_SRC_BUDGET=2048`/源/次;整批不拆;
**打满的源 dirty 位必须置回**(丢位 = 风暴尾批永久搁浅);按源预算
天然公平无需轮转态。**验证:gaps 2-6 → 0;最坏迭代 1639ms →
50-66ms(park 超时地板);ZADD 3.52 → 3.47/3.42M/s 带内。**
副产品:perfgate zadd 掷硬币 REFUSED 根除,median-of-N 门禁变可行。

**学费**:①症状在连接层、根因在 drain 层 —— 心跳探针(每 shard
每圈 wall)入标准工具箱 ②`cargo check` 0.07s "Finished" 是缓存判决
不是编译,发盒前要真 build(盒上逮住 field 名错)。finding =
`bench/PERF-FINDING-2026-08-07-zadd-pause-drain-starvation.md`。
r1-locality 现 **+25 笔**(tip `c625b585`)。

---

## 十二期(2026-08-07,perfgate-median 落地 —— 定稿账本)

**`bench/perfgate-median.sh` 入库**(`59562cb8`,先实证产出后入门禁,
按既有纪律):N 轮 perfgate、每角 median(cand) vs median(ref)、
REFUSED 即中止(部分集的中位数不是中位数)。**首次 N=3 全程零
REFUSED**(drain 预算修复在门禁上兑现)。

**定稿账本**:get −0.8 / set −4.4 / lpush −5.5 / cluster/compat 绿 /
zinterstore +9.0 全 PASS;**sadd −10.6 / hset −9.7 / zadd −13.9 /
incr −10.4 中位数 FAIL** —— 单跑高点(sadd −7.8)与低点(zadd −15.7)
都是带缘,此表是与 alloc-off 参照的确定距离,残余 RFC 三门现在有了
精确定价基准。r1-locality 现 **+27 笔**(tip `f47cceb9`)。

---

## 十三期(2026-08-07,泛化核验 —— zadd 全同,incr 半逃逸)

owner 线程 ON/OFF 双 verb 采样:**zadd 精确复刻 hset**(allocator
13.3%→23.5%,+10.2pp ≈ 其 −13.9 中位税)—— 快路径故事覆盖全部集合写。
**incr 不吻合**:allocator 仅 +3pp(6.7→9.7),其 −10.4 中位数大半
薄摊在全符号;且 incr 历史带最宽(−2.5…−11.5),n=3 中位数不宜过度
解读,机制猎捕前先加大 N。**RFC 门 B 修正**:99.9% 命中率下 claims
加宽动机弱;转发路径若有刀 = 单发 Request/Response 的信封池化
(批路径 argv husk 已池化)。r1-locality 现 **+28 笔**(tip
`0947c951`)。弧图补完,全部余项归属主。

---

## 十四期(2026-08-07,incr 定性除名 —— 弧全闭)

8 轮插值 ON/OFF:ratio 中位 **≈0.99**、带 **0.878-1.317** ——
**incr 无可证明的 allocator 税**;perfgate −10.4 中位 = 其宽带在 n=3
撞上(自身也在漂的)ref 基。两轮双侧绝对值同时掉 40% = 共享盒干扰
可见(插值保 ratio 不保带宽)。incr 从机制猎捕除名(噪声主导角;
未来宣判需净盒或 N>>8)。**真实余距 = sadd/hset/zadd 三集合角
(机制已全确认 = 快路径每调用成本)**。r1-locality 现 **+29 笔**
(tip `b07d7bba`)。**快路径弧至此全闭:所有角有机制归属或噪声定性,
所有余项是拍板件。**

---

## 十五期(2026-08-07,T3 开工 —— kevy-compress v1 落地)

快路径弧全闭后转入唯一开放跑道。**`kevy-compress` v1 入库**
(`fa3d655c`,444 行核心 + 144 行测试,`forbid(unsafe_code)`,
no_std+alloc,零依赖):LZ4 形 token 流(单探哈希/16 位偏移/无界长度)
+ **字典作虚拟前置历史**(K4 的机制)+ raw 兜底(K2 结构化)+
全边界检查解码器。

**对着实测判据交卷**:K1 = **16.7 GB/s 解码**(地板 1GB/s,16× 余量;
结构化 4KiB 压至 100B = 40.96×)/ K2 = 随机输入落 TAG_RAW,帧 ≤
输入+6 / K3 = 截断/翻位/未知 tag 全拒 / **K4 = 1000×400B 相同值
对共享字典 ≤16B/值**(单测钉死 O(dict)+N×small 形状)。`train()` =
稳定签名后的可替换策略(K4 finding:构造是胜负手)。

**下一切片**:RFC §5 布点(demote 编码 / compact 重编 / 冷读解码),
K5/K6/K7 骑既有门禁;涉 kevy-store/kevy-vlog,爆炸半径大,单独一轮。
workspace 207 套件绿。r1-locality 现 **+30 笔**。

---

## 十六期(2026-08-07,T4 布线 —— vlog 每记录一帧,字典随文件生死)

RFC §5 落位在 **vlog 层**(`066da155`):encode 只存在于
`Vlog::append` 内 → **K6 构造性成立**(SET 路径不可达);每
`VlogFile` 携带自己的字典 —— 轮转时用**上一文件的原始样本**训练
(§7.2 轮转播种:同种群、真字节、无冷启动窗),随文件消亡(**K7
可弃性是继承的,不是工程的**);compact 跨文件自动 decode→按目标
字典 re-encode(§3 白得)。盘上封框不动:帧在 body 内,CRC 恰盖
存储字节;`verify_image` 交帧,`VlogFile::decompress` 是完成步
(tier_serve 批读点配对)。487 行 lib.rs 越限 → compaction 拆
`compact.rs`。

**测试**:kevy-vlog 14(新:字典跨轮转生效 —— 400B 值在带字典文件
塌缩至 <1/4 raw;stats.bytes == Σdisk_len EXACT)/ 四处常量值形状
测试改噪声载荷(压缩改写了密封与大小前提 —— 本身就是布线生效的
证据)/ fuzz 双靶入库 / **compressgate T3 五线转绿**
(K2/K3/K4/K5-identity/K6),K1/K5-amp/K7 留待 envelope。
workspace 连续两轮全绿(persistence flake 未复现)。

**下一切片(T4 收口)**:lx64 capacity-envelope —— K1(压缩开启下
B2 冷读 p99)/ K5-amplification(可压缩语料 vs B5 1.27×)/ K7
(B10/B11)+ perfgate KV/pubsub 不退化线。r1-locality 现
**+31 笔**(tip `066da155`)。

---

## 十七期(2026-08-07,T4 envelope 收口 —— compressgate 八线全绿)

lx64 全刻度 capacity-envelope 于 T4 tip **全 phase PASS**(学费:
/tmp 是 tmpfs,envelope 自带拒绝,TMPDIR 指盘即过):**K1 = 冷读
p99 128µs 标量 / 130µs 行**(预算 300/500,解码埋在带内;无压缩
基线 ~105µs)/ **K5-amp = 0.01×**(同值语料 ~19.4GB 冷字节 →
273MB vlog ≈ 71×;无压缩基线 1.27×)/ B6 10.1× / B8 预算 ✓ /
sweep 14/14 / D1-D4 全绿。**compressgate 八条 K 线全部真断言全绿**
(K1/K5-amp 消费 envelope 结果文件 = tiergate 模式;K7 = 字典纯
内存字段 + 可弃性契约测试)。

**两条诚实 caveat 已入 finding**(`fe4948ae`):① envelope 语料 =
单值重复 = K4 类别主张的**天花板端**,现实中位 = K4 前提表的
templated-JSON 2.2×,对外口径保持定性 ② amp 0.01× 下压实阈值
从未触发(epoch=0),compact 终止性本轮仅单测覆盖。

**T3+T4 弧闭**:kevy-compress(石头)+ vlog 布线 + 八线门禁,三轮
落地。r1-locality 现 **+32 笔**(tip `fe4948ae`)。ROADMAP 的压缩
train 余项 = 熵编码档(RFC §7.1 具名 follow-up)与真实消费者语料
测量(mailrs),均非本轮。

---

## 十八期(2026-08-07,oracle 对表 + train() 第一课)

`examples/k4_corpora.rs` 把 K4 前提四语料过真 codec 对 zlib oracle
对表(B/值):**identical 去重后 9.4 vs oracle 41.8(反超 4.4×** ——
整值字典命中一个 token vs zlib 每记录 deflate 头)/ templated
264 vs 180 / textual 241 vs 104 / random 447 vs 406。三个事实:
① **train() 第一次被测量教育**:未去重版把 164 份同值塞进字典
(65B/值摊销买零捕获)→ FNV 精确去重落地(碰撞无害)
② **对 oracle 残差 = 字面量熵编码**(dict 捕获本身在工作:templated
payload 354→199)—— §7.1 具名 follow-up 档现在有了按语料的定价
③ 不可压语料上字典是纯成本(vlog 尺度 0.02%,记录不动作)。
workspace 全绿。r1-locality 现 **+33 笔**(tip `b79ea0dd`)。

**压缩 train 可 autorun 的余项已尽**;熵编码档是"再来一遍同量级
工作"(RFC 原话),开工与否归属主。
