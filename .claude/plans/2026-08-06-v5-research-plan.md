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

**R5 ▶ 未做**(pubsub 残余 profile;同款手法,pubsub_bench 驱动)。
**R6 ▶ 未做**(K1 解码预算量化)。两者是下轮研究的开口。

