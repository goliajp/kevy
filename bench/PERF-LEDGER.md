# PERF-LEDGER — kevy vs FOSS 真 gap 账本

> ## ⚠️ 全表精度警告(2026-07-12)
>
> **本文件里所有用 `redis-benchmark --threads` 测出来的数字,都被量化到
> 250ms 的格子上,并且被系统性低报。** 机制:`--threads` 下 redis-benchmark
> 的唯一出口是它自己 250ms 的 `showThroughput` 定时器
> (`redis-benchmark.c:52 SHOW_THROUGHPUT_INTERVAL`、`:1653 aeStop`;不加
> `--threads` 时是 `:425` 在 `clientDone` 里立即停),于是 `totlatency`
> (`:970/:973`)被**向上取整**到 250ms 的整数倍。
>
> 自己验:把任一 arena 读数换算成耗时(`N / rps`,N=8M):
> GET 6,389,776 → **1.2520 s**;SET 5,326,232 → **1.5020 s**;
> HSET 3,998,001 → **2.0010 s**;LPUSH 3,196,164 → **2.5030 s**;
> ZADD 2,666,667 → **3.0000 s**;valkey GET 2,131,628 → **3.7530 s**;
> valkey SET 1,598,082 → **5.0060 s**。**每一个都是 250ms 的整数倍。**
> 这就是为什么 GET 和 SET 读数逐位相同、INCR 和 SADD 读数逐位相同 ——
> 它们落在同一个格子里,不是巧合,也不是抄错。
>
> arena 口径(elapsed ≈ 1.25 s)**一格 = 20%**;perfgate legacy 口径
> (≈ 3.5 s)**一格 = 7%**。
>
> **仍然成立的**:每个结论的**方向**。kevy 和 valkey 在每个面上都隔着三到五
> 个格子,250ms 的取整填不平 3× 的差距。**死掉的是精度** —— "3.00×"、
> "±2.8k"、"−18.7%" 这些小数点没有意义。
>
> `bench/perfgate.sh` 与 `bench/arena.sh` 已改为**从服务端自己的命令计数器
> 上、在稳态窗口里读吞吐**(两个引擎的 `INFO total_commands_processed` 同义,
> 竞品对比依然成立)。**下面的表在用新测法重跑之前,只应被当作"方向正确、
> 精度不可信"来读。** 详见
> `PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md`。

**状态:v3.8.0 定稿(2026-07-05)**。复测节奏:每个 release 前全矩阵
重跑;对标物升级(valkey / redis-stack 新 GA)= gap 表重测。
裸面 perfgate ratchet 的盒级环境阻塞档案见
PERF-FINDING-2026-07-05-tails-closure.md(floor 不下调,盒恢复复验)。

初版:2026-07-05(v3.3 基线 arena)。对标物版本入账;gap 规则:
差值 ≤ max(两侧 stdev) = NOISE。协议:lx64,隔离(单服务器占同核),
host loopback,client 绑异核,median-of-5 + sample stdev。

## ★ v4.0 arena —— 第一张用没有量化的尺测出来的表(2026-07-12,lx64)

**协议**:`-c 50 -P 16`,server 绑核 0-7 / client 绑核 8-15,四引擎逐个独占同一批核,
median-of-5。**吞吐从服务端自己的 `INFO total_commands_processed` 上、在 1s ramp 之后的
3s 稳态窗口里读**,不再读 redis-benchmark 那个被 250ms 定时器向上取整的 rps
(四个引擎同一把尺;机制见 `PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md`)。

| 命令 | kevy 4.0 | valkey 9.1 | redis 8 | dragonfly | vs valkey | vs redis 8 | vs dragonfly |
|---|---:|---:|---:|---:|---:|---:|---:|
| GET | **7,800,299** ±138k | 3,014,687 ±52k | 5,597,865 ±248k | 2,132,210 ±19k | **2.59×** | **1.39×** | **3.66×** |
| SET | **6,918,058** ±225k | 1,749,976 ±46k | 2,573,396 ±75k | 1,511,377 ±329k | **3.95×** | **2.69×** | **4.58×** |
| INCR | **6,133,940** ±402k | 2,484,273 ±74k | 3,459,395 ±53k | 1,387,568 ±152k | **2.47×** | **1.77×** | **4.42×** |
| SADD | **5,600,597** ±485k | 2,385,857 ±17k | 3,690,483 ±43k | 1,678,098 ±230k | **2.35×** | **1.52×** | **3.34×** |
| HSET | **4,287,217** ±203k | 1,970,791 ±34k | 3,021,325 ±91k | 1,515,763 ±233k | **2.18×** | **1.42×** | **2.83×** |
| LPUSH | **3,213,470** ±87k | 1,943,222 ±32k | 2,862,374 ±85k | 1,320,497 ±33k | **1.65×** | **1.12×** | **2.43×** |
| ZADD | **3,053,101** ±215k | 1,802,759 ±109k | 2,773,929 ±119k | 1,455,126 ±130k | **1.69×** | **1.10×** | **2.10×** |

**7/7 全胜,对全部三个对手。** 区间:vs valkey **1.65–3.95×**、vs redis 8 **1.10–2.69×**、
vs dragonfly **2.10–4.58×**。

### 这张表跟旧表的差别 —— 我们之前高估了自己

去掉量化之后,**kevy 的绝对值涨了**(GET 6.39M → **7.80M**,+22%;量化一直在低报我们),
**但对手涨得更多**(valkey GET 2.13M → 3.01M,+41%;redis 8 GET 4.00M → 5.60M,+40%)。
于是**倍数缩水**:

| | 旧(量化桶算出来的) | **真值** |
|---|---|---|
| GET vs valkey | 3.00× | **2.59×** |
| GET vs redis 8 | 1.60× | **1.39×** |
| GET vs dragonfly | 3.60× | **3.66×** |

**必须如实印出来的一格**:**LPUSH 1.12× / ZADD 1.10× vs redis 8** —— 几乎打平。
按本表的 gap 规则(差值 ≤ max(两侧 stdev) = NOISE)这两格仍算真赢
(LPUSH 差 351k > stdev 87k;ZADD 差 279k > stdev 215k),但**领先幅度是个位数百分比级别**,
再也不能用"全面碾压"来描述对 redis 8 的写路径。

**"1.60× redis 8"这个说法自本表起作废。**

## 对标物

| 对标物 | 版本 | 来源 |
|---|---|---|
| valkey | 9.1.0(io-threads 8) | docker valkey/valkey:9.1 |
| redis-stack-server | Redis 7.4.7 + RediSearch(image 3b02c41b1595) | docker redis/redis-stack-server:latest |
| kevy | 3.0.0-dev(develop @ v3.2)| release-perf,8 shards |

## 裸面(redis-benchmark,-c50 -P16,7 类)— **全胜 1.6-3.3×**
(v1.1 校正 2026-07-05:v1 的 2M/cell 短跑量子化低估了 kevy;
8M/cell ≥2s 定真值。v3.4 上限阶梯佐证:client 加宽不再抬升 =
server-bound 真值。)

| test | kevy(8M/cell) | valkey | 比 | 判定 |
|---|---|---|---|---|
| GET | 6,394,884 ±478k | 2,131,628 ±69k | **3.00×** | WIN |
| SET | 5,329,780 ±787k | 1,599,041 ±46k | **3.33×** | WIN |
| INCR | 5,326,232 ±789k | 2,131,628 ±83k | **2.50×** | WIN |
| HSET | 3,996,004 ±360k | 1,776,199 ±47k | **2.25×** | WIN |
| SADD | 5,326,232 ±608k | 2,131,628 ±0.7k | **2.50×** | WIN |
| LPUSH | 2,909,091 ±303k | 1,776,199 ±0.2k | **1.64×** | WIN |
| ZADD | 2,906,977 ±226k | 1,682,440 ±0.5k | **1.73×** | WIN |

kevy 侧 stdev 7-15%(高吞吐下 run 间波动);全部 gap 仍远超
max(stdev) = 真赢。

## Serving 面(200k 同种子语料:Zipf 文本 / 流形 128d 向量 / Zipf 组;
kevy 四索引 vs RediSearch 单复合 FT 索引;200 查询 × median-of-5)

| class | kevy p95 / qps | stack p95 / qps | 判定 |
|---|---|---|---|
| FTS(MATCH vs FT.SEARCH BM25) | 6.588ms / 330 | 6.562ms / 273 | p95 **NOISE**(Δ0.4% < stdev);qps kevy +21% WIN |
| ANN(KNN vs FT KNN HNSW) | 4.354ms / 234 | 1.138ms / 806 | **stack WIN 3.8×** ← 本 arc 唯一真 gap |
| AGG(GROUPS vs FT.AGGREGATE) | 1.851ms / 550 | 202.9ms / 5 | **kevy WIN 110×**(写时聚合 vs 查询期聚合,架构级) |
| NUMERIC(RANGE+FIELDS vs @val:[a b]) | 0.190ms / 5338 | 0.428ms / 3014 | **kevy WIN 2.3×** |

构建时间:kevy 四索引 47.3s vs stack 单复合索引 82.4s(kevy WIN)。

**ANN gap 修正(v3.6 A0,2026-07-05 pareto 对齐,100k 向量 +
FLAT 精确 oracle)**:名义 3.8× 大部分是 **EF 语义伪 gap** ——
kevy 的 EF 逐 shard 生效(cmd_index_query.rs:315,8 shard = 8×
等效 beam 工作),同名义 EF ≠ 同 recall。同 recall 档真账:

| recall | kevy | stack | 真 gap |
|---|---|---|---|
| 1.000 | EF50 @ 0.837ms | EF400 @ 0.791ms | **1.06× ≈ 平手** |
| ~0.99 | EF20 @ 0.491ms | EF100 @ 0.263ms | 1.87× |
| 底档 | EF16 @ 0.445ms | EF20 @ 0.113ms | ~4× 地板 |

**v3.6 campaign 终账(3 攻 3 中,profile 驱动,累计 -40~-43%)**:
attack1 epoch 戳记访问池(SipHash 出局,-9~-17%)→ attack2 8 通道
距离核(标量归约链→自动向量化,-26~-29%)→ attack3 AVX2+FMA
运行时分发(-7~-11%)。§9 gate 立功:fan-out 管线假说被 profile
翻盘(63% 在 beam search 内核,不在管线)。

| recall | kevy(3 攻后) | stack | 判决 |
|---|---|---|---|
| 1.000 | EF50 @ **0.481ms** | EF400 @ 0.791ms | **kevy WIN 1.64×** |
| 0.99 | EF20 @ 0.288ms | EF100 @ 0.263ms | 1.10× ≈ 平 |
| <0.98 超低延迟带 | 不可达(EF≥16 × 8-shard 最小功)| 0.113ms | fan-out 架构地板(stateless-shard 不破,REFUSED) |

kevy 可达的每个 recall 档均已 WIN 或平;唯一让出的是
sub-0.98-recall 超低延迟带(应用侧极少:那档 recall 不足以 serving)。

## Gap 表 → train 修订(2026-07-05 评审,写回 ROADMAP)

- v3.4:裸面无 gap → 改为 **tails 清偿 + client-bound 上限复测**
  (286c4a2 -4% micro / epoll stay-hot 对称 / IDX conn-tail 根因)。
- v3.5:FTS 无 gap(qps 已胜)→ **巩固站**:单常见词 postings scan
  (impact-ordering,自 ratchet 改进非 gap 驱动)。
- **v3.6:ANN 攻坚 = 本 arc 主战场**(3.8× 真 gap;pareto 对齐 →
  Phase A decomposition vs RediSearch HNSW 实现 → Phase B)。
- v3.7:agg/query 全胜 → 缩编并入 v3.8 终账(全部 arena 线 ratchet 化)。

## v3.17.0 release arena — bare face 复测(2026-07-06,lx64,per-release 承诺)

kevy 3.17.0 vs valkey 9.1.0,fair-fight 协议(-c 50 -P 16 -n 8M,
server 0-7 / client 8-15,median-of-5)。可用性 arc 全落地
(READONLY gate / ACK / 心跳 / failover / 一致性阶梯 / lease)后的
裸面确认:**7/7 全胜,无一 NOISE 格**。

| test | kevy | valkey | ratio |
|---|---|---|---|
| GET | 6,389,776 | 2,131,060 | **3.00×** |
| SET | 6,389,776 | 1,598,082 | **4.00×** |
| INCR | 5,326,232 | 2,131,628 | **2.50×** |
| SADD | 5,326,232 | 2,131,628 | **2.50×** |
| HSET | 3,998,001 | 1,776,199 | **2.25×** |
| LPUSH | 3,196,164 | 1,776,199 | **1.80×** |
| ZADD | 2,666,667 | 1,682,793 | **1.58×** |

复制/心跳/gate 管线不吃裸面吞吐(v3.14-v3.16 增量 = 0 回归)。
charter「对标并远超 valkey」在终版复核成立。

## v4 T4 K-401 — LPUSH 观察项收口(2026-07-11,lx64,feature/v4 @ f7585650)

v3.18.0 复测记的观察项(LPUSH 2.91M vs v3.17.0 的 3.20M,-9%)按
arena 同协议 ×3 轮独立 fresh instance(每轮 median-of-5)收口:

| round | LPUSH median | stdev |
|---|---|---|
| 1 | 2,905,921 | ±159k |
| 2 | **3,196,164** | ±159k |
| 3 | 2,906,977 | ±159k |

**判定:非回归,观察项关闭。**(⚠️ 2026-07-12 更正:结论对,**理由错**。
所谓"instance 级双峰"是 redis-benchmark 的 250ms 计时量化 —— 2.91M→2.7530s、
3.20M→2.5030s,相邻两格。不是 page placement,不是 IRQ 运气。见
`PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md`。) 同一 binary 三轮
fresh instance 在 2.91M / 3.20M 两个模式间跳——round 2 逐位复现
v3.17.0 的 3.20M,round 1/3 复现 v3.18.0 的 2.91M。两个"版本值"
都是同一 instance 分布的两个峰(page placement / IRQ luck;
legacy8sh SET 双峰同款,见 PERF-FINDING-2026-07-03-legacy8sh-set-
bimodal.md)。v3.17→v3.18 的 -9% 是单轮 arena 各采到一个峰的读数
差,不是代码回归。vs valkey 1.64-1.80× 领先不变。

后续盯法:LPUSH(连同 INCR/SADD/HSET/ZADD)自本轮起进 perfgate
legacy_8sh_* ratchet(K-402)——观察项从人工复测改为每 gate 自动化,
floor = 基线 ×0.92 吸收双峰带宽。

## v4 T4 K-402 — ⚠️ **已撤回(2026-07-12):不存在回归,是尺子** (原标题:gate 抓到 v4 SET 写路径真回归)

> **撤回理由**:①`legacy_8sh_*` 角用 `--threads`,而 redis-benchmark 在
> `--threads` 下的唯一出口是它自己 250ms 的 `showThroughput` 定时器
> (`redis-benchmark.c:52`/`:1653`),elapsed 被向上取整到 250ms 的整数倍 →
> 吞吐被**量化成 7% 的格子**。把下表的数换算回耗时:9.21M→3.2560s、
> 8.56M→3.5070s、7.49M→4.0060s —— 所谓"分布零重叠"就是相邻的两个格子。
> ②A/B 的顺序是固定的(v3.18.0 的三个 instance **先跑完**再跑 v4),
> 而盒子在长跑中单调下漂 → 先跑的系统性偏高。这一条同时解释了 pinned 角
> (它们**不用** `--threads`,不受量化影响)。
>
> **2026-07-12 顺序平衡 + 修好尺子后复测**:legacy SET **−0.23%**、
> legacy GET **+0.98%**、pinned_cluster_set **−1.0%**,全部落在样本方差
> (3.5–5.6%)之内。**没有回归。** 下面的两段 bisect 是在给一个假象拟合机制。
> 详见 `PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md`。

perfgate 新增 legacy 拓扑 5 角(INCR/SADD/HSET/LPUSH/ZADD,
redis-benchmark stock -t,同 get/set N=30M ×3 instances);
legacy_8sh 双角重录。f7585650 全量 measure 中三条 SET 角远低
baseline → 按 Pre-Phase-A gate 做同盒同小时 A/B(vs v3.18.0 tag
+ 中点 8910ba84),**证实 feature/v4 SET 写路径真回归,GET 三角
零回归**:

| SET 角 | v3.18.0 | 8910ba84(pre-K-110) | f7585650 | 累计 Δ |
|---|---|---|---|---|
| legacy_8sh_set | 9.21M | 8.56M(-7.1%) | 7.49M(再 -12.5%) | **-18.7%** |
| pinned_cluster_set | 22.52M | — | 20.82M | **-7.5%** |
| pinned_compat_set | 17.00M | — | 16.05M | **-5.6%** |

两段回归:T1a 实例化/K-108 段 + K-110 内核判决段。arena -P16 口径
不显(SET 6.39M 持平 v3.18)——深管线(P256)才暴露的 per-op 成本。
细账 + baseline 处理纪律见
PERF-FINDING-2026-07-11-v4-set-write-path-regression.md。

baseline 合成(不为绿灯改账):legacy_8sh_set 重录为 v3.18.0 同小时
真值 9,210,970(消陈旧双峰误报,保留回归红灯);legacy_8sh_get
10,877,494(逐位同旧);5 新角以 f7585650 实测入 ratchet 起点;
pinned 4 角 + zalg 保留 2026-07-03 记录不动。**feature/v4 上 gate
= 9 绿 / 3 红(三条 SET 角)——红灯即 finding,修复(decomp →
attack)待用户拍板排程。**

## v4.0.0 release arena — bare face 复测(2026-07-19,lx64,**服务端计数口径**)

本表**第一次不受顶部精度警告约束**:吞吐读的是各服务端自己的
`total_commands_processed` 在计时窗口内的增量,不再是 redis-benchmark
自报速率,250ms 的格子消失。原始输出:[`ARENA-2026-07-19.txt`](ARENA-2026-07-19.txt)。

**7/7 全胜 valkey 9.1**(median-of-5,gap 全部远大于两侧 stdev,无 NOISE 格):

| op | kevy | valkey 9.1 | 比值 | v3.18.0 旧口径 |
|---|---:|---:|---:|---:|
| GET | 7.24 M/s | 2.95 M/s | 2.46× | 3.00× |
| SET | 6.67 M/s | 1.67 M/s | 4.00× | 3.99× |
| INCR | 6.20 M/s | 2.17 M/s | 2.86× | 3.00× |
| SADD | 6.25 M/s | 2.12 M/s | 2.95× | 2.50× |
| HSET | 3.72 M/s | 1.72 M/s | 2.17× | 2.25× |
| ZADD | 2.98 M/s | 1.67 M/s | 1.78× | 1.73× |
| LPUSH | 3.07 M/s | 1.75 M/s | 1.76× | 1.64× |

四引擎 GET 面(kevy 7.24 M/s):valkey **2.46×** / redis 8 **1.25×** /
dragonfly **3.48×**(README 此前公布 3.00× / 1.60× / 3.60×,已同步下修)。

**比值有升有降,不是引擎变了,是尺子换了。** 旧口径把两侧都低报,
且低报程度按引擎不同,比值因此失真——LPUSH/SADD/ZADD 升,
GET/INCR/HSET 降。kevy 自己的绝对值全线上升(GET 6.39 → 7.24 M/s)。
**v3.18.0 那节留下的 LPUSH 观察项(2.91M,较 v3.17.0 低 9%)在此闭合**:
新口径 3.07 M/s,vs valkey 1.76×,高于旧口径的 1.64×。

## v3.18.0 release arena — bare face 复测(2026-07-10,lx64)

结构 arc(LOC 还债 + 热路径批 D + fuzz/polish)后的裸面确认:
**7/7 全胜 valkey 9.1**(GET 6.39M = 3.00×,SET 6.38M = 3.99×,
INCR 3.00×,SADD 2.50×,HSET 2.25×,ZADD 1.73×,LPUSH 1.64×)。
GET/SET 与 v3.17.0 基线持平(批 D perfgate ×3 median 已单独验证
热路径拆分零回归)。注:LPUSH 2.91M 较 v3.17.0 的 3.20M 低 9%
(超该格 stdev 4.1%,共享盒单轮;vs valkey 仍 1.64× 领先),
下一 release 复测观察——不构成 ship 阻塞,构成观察项。

## v4 post-fix arena — bare face 复测(2026-07-22,lx64)

ROADMAP t5 的悬案项。FTS arc 全量落地(doc values / IN / FILTER / SORT /
DISTINCT / FACET)后的裸面确认,同盒同协议:median-of-5、服务端命令计数器
读吞吐、server 核 0-7 / client 核 8-15。盒况记账:16 核、load 0.61、其余
容器全部 ~0% CPU(共享盒,未停任何他人服务)。原始数据
[`ARENA-2026-07-22.txt`](ARENA-2026-07-22.txt)。

**kevy 自身:7 格里 6 格是噪声,1 格真涨。** 按本文件的 gap 规则
(|Δ| ≤ max(两侧 stdev) 即 NOISE)对照 2026-07-19：

| cell | 07-19 | 07-22 | Δ | 判定 |
|---|---:|---:|---:|---|
| GET | 7.24 M | 7.16 M | -1.1% | NOISE |
| SET | 6.67 M | 6.58 M | -1.3% | NOISE |
| INCR | 6.20 M | 6.60 M | +6.5% | NOISE |
| HSET | 3.72 M | 4.02 M | **+8.1%** | **REAL** |
| SADD | 6.25 M | 5.91 M | -5.5% | NOISE |
| ZADD | 2.98 M | 3.01 M | +1.0% | NOISE |
| LPUSH | 3.07 M | 2.95 M | -4.0% | NOISE |

索引侧整条 arc 落地后裸面零回归,HSET 还真涨了一格。

**但比值不能跨会话比,这轮把原因坐实了。** 同一 valkey 镜像
(`valkey/valkey:9.1`,digest `sha256:4963247a…`,2026-05-19 构建，两轮
同一个），GET 从 2.95 M 涨到 3.41 M（+15.8%）。两轮 stdev 都紧
(66k / 29k)、区间不重叠——不是 run-to-run 噪声，是**两次会话之间盒子状态
的差异**，而 07-19 那次的状态无法重建。

所以**唯一可信的是同一会话内的比值**。今天这一会话：

| cell | kevy | valkey | ratio |
|---|---:|---:|---:|
| SET | 6.58 M | 1.70 M | 3.86× |
| INCR | 6.60 M | 2.42 M | 2.73× |
| SADD | 5.91 M | 2.40 M | 2.46× |
| GET | 7.16 M | 3.41 M | 2.10× |
| HSET | 4.02 M | 1.95 M | 2.06× |
| ZADD | 3.01 M | 1.83 M | 1.64× |
| LPUSH | 2.95 M | 1.90 M | 1.56× |

**7/7 仍全胜**，redis8 与 dragonfly 亦全胜（原始数据同上）。

**README 未改，等拍板。** 表上现挂的是 07-19 那轮的 GET 2.46× / SET 4.00×，
今天同会话是 2.10× / 3.86×。两个都是真测量，差异来自对手侧而非 kevy——
在那 15.8% 的来源查清之前，把公开性能声明换成一个我解释不了的数字，比留着
一个注明了日期的旧数字更糟。要么补一轮同盒复测定夺，要么改成"最近一轮同会话
比值"并注明口径；这是对外声明，留给用户拍。

## v3.4 悬案闭合:arena 的 kevy 数字是服务端上限,不是客户端上限(2026-07-22)

ROADMAP v3.4 挂了很久的一条:「arena kevy 数字疑似 client 打满;加宽 client
定真值入 ledger」。**测了,疑似不成立。**

探针 [`clientbound.sh`](clientbound.sh) 用 arena 的原方法(服务端
`total_commands_processed` / 自计时窗口),只多做一件事:把负载生成器自己的
`utime+stime` 也在同一窗口内采样,于是**饱和是观察到的,不是推断的**。
服务端固定 8 核,只扫客户端宽度,GET,median-of-3:

| 客户端核 | 线程 | ops/s | 生成器 CPU% |
|---|---:|---:|---:|
| 8-15(arena 点) | 6 | 7,229,130 | 592 |
| 8-15 | 8 | 7,368,886 | 666 |
| 8-15 | 16 | 7,240,523 | 681 |
| 4-15 | 12 | 7,015,257 | 650 |
| 4-15 | 24 | 7,225,262 | 698 |

**线程 4×、核 1.5×,吞吐平的**(7.02–7.37 M,±2.5% 内),且生成器 CPU 峰值
698%,8 核里始终留着余量;把核从 8 加到 12 反而没抬起来。**加宽客户端抬不动
的数,不是客户端的天花板。** 7.2 M 是 kevy 在这套协议、这 8 个服务端核上的
**服务端上限**,arena 的比值分母因此是真的。

顺带:arena 点复现今日 arena 的 GET 7.16 M(在噪声内),两个 harness 互证。

**这条不解释 valkey 的 15.8%**,但排除了一个假设:客户端在 kevy 7.2 M 时都
没打满,在 valkey 3.4 M 时更不可能是限制因素。那个跨会话差异仍未归因,README
的对外比值仍不动。

## 短语查询 -6.2%:剖面指对了面,而先猜的那次指错了(2026-07-23)

ROADMAP t5.7 的 `5d.2 phrase head+head`。**它的诊断是错的,实测才发现。**
条目写着「head+head 仍扫 head 表,需 positional galloping」——真正的成本不在
扫描。

**先猜,被否。** 位置表解码出来本就升序,而 `phrase_starts` 把它塞进
`HashSet` 再求交,看着是浪费。改成双指针归并:88 测试绿、clippy 净、
**实测 94.41 → 104.17ms,慢 10.3%**。回滚。

**再分解,看见的却是别的东西(2026-07-23 更正)。** 当时的 `perf record` 报
**87% self-time 在 libc 分配器**,据此判定短语路径「分配受限」。**这个剖面是
错的**:它抓到的是分片**拆除**阶段——释放上百万条位置 blob 的 `free()` 风暴
——而不是查询。四个各自独立的取证缺陷叠加所致(`strip=true` 抹掉符号 /
`-- sleep N` 没能约束窗口 / `pgrep|head -1` 挂到了 `su` / gate 的 python 块
缓冲让"建索引完成"标记迟到到关机),四次的数字都看着合理。四条修复与自检已
落在 `bench/profile-textgate.sh`。

真源头仍然存在,只是量级小得多:`Positions::get` 每次调用把 blob 解码进一个
新 `Vec`,head+head 每查询数十万次分配。消掉它值 **-6.2%**(见下),而不是
87%。

**按剖面攻。** 加 `Positions::blob()`(交回裸字节)+ `walk()`(原地走
delta+varint),并利用一个此前忽略的事实:**未限定字段时只要一个布尔**,于是
判定短路在第一个命中,零分配。看着像二次方的"每个起点重走后面词的 blob"在真实
形状下不是——blob 是**单文档单词**的出现位置,几乎总是一两个。

**五对交替配对,每轮记录盒子忙闲**:

| 对 | base | noalloc |
|---|---:|---:|
| 1 | 92.79 | 87.02 |
| 2 | 92.69 | 87.50 |
| 3 | 93.10 | 87.91 |
| 5 | 92.60 | 86.96 |
| 6 | 93.09 | 87.06 |

两臂区间**完全不重叠**(92.60–93.10 vs 86.96–87.91),**-6.2%**。

**第 4 对被剔除,依据不是"不好看"**:base 93.97/worst 108、noalloc 158.73/worst
227——**两臂同时劣化**。查盒子抓到 `rustc` 99.8% + `Runner.Worker`。

### ⚠️ 由此暴露的测量前提:**bench 盒同时是 self-hosted CI runner**

`release.yml` 的 `runs-on: [self-hosted, lx64]` 让 lx64 既跑基准又跑 CI。
**lx64 上任何测量都可能被落到同一台盒子的 job 静默污染**——arena 的对手比值、
五个 textgate 门,全部适用。单臂异常可能是被测对象,**两臂同劣是盒子**;此后
交替配对 + 每轮记 `pgrep -c "rustc|Runner.Worker"`,离群点当场可判而不必事后追查。

**galloping 仍未验**:5d.2 原提的 skip-list/galloping 在分配消掉之后还有没有
剩余收益,**没测过**,不写成"已做"。

---

## 2026-07-23 — 文本查询 **-73%**:每次查询都在丈量索引占多少内存

**上条的真正续章。** 上面那份「87% 在分配器」的剖面被更正为拆除阶段之后,用
修好的取证脚本重录了一份**干净的查询期剖面**(`drop_glue: 0`),画面完全不同:

| self-time | 符号 | 属于 |
|---:|---|---|
| 49.63% | `docblobs::channel_bytes` | 内存估算 |
| 32.52% | `segment_stats::TextSegment::stats` | 内存估算 |
| 3.99% | `kevy_rt::uring_reactor::run_uring` | reactor |
| 2.68% | `segment_phrase::matches_query_faceted` | **真正的检索** |
| 1.77% | `positions::Positions::blob` | **真正的检索** |

**82% 的查询 CPU 在做内存会计,真正的检索只有 4.5%。**

**根因一句话**:跨分片 BM25 的 pass-1 每个分片只需要一个文档数,却调用了
`TextSegment::stats()` —— 它顺带算 `approx_bytes`(遍历每个词、每条 posting、
每个位置 blob),然后把结果**扔掉**。索引越大,每次查询白走的路越长。

**改法**:加 `TextSegment::docs()`(就是一次 `len()`),三处调用点改用它
(server pass-1 / embedded pass-1 / IDX 基数)。`stats()` 保持原样给 IDX.INFO
—— 那才是真正想要它的调用者。

**三轮交替配对(1M 文档,`"w0 w1"` head+head 短语)**:

| 轮 | before | after |
|---|---:|---:|
| 1 | 87.43 | **23.51** |
| 2 | *(该轮 before 未出 p95 行)* | **23.58** |
| 3 | 86.98 | **23.38** |

**phrase p95 87.4 → 23.5ms,-73%(3.7×)**。两臂区间相距近四倍,无重叠。

### 教训

- **取证工具坏了四次,四次的数字都合理。** 若不是坚持追「谁在调用分配器」,
  这条 82% 的会计开销会一直躲在「分配受限」这个看似合理的结论背后。剖面的
  自检(出现 `drop_glue` 即判窗口打在拆除上)比剖面本身更值钱。
- **`stats()` 这类"顺带全算"的聚合函数是热路径陷阱**:调用方只要一个字段,
  代价却是整个结构。给热路径留**窄入口**,别让它借宽接口。
