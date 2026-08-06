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

