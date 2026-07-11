# PERF-LEDGER — kevy vs FOSS 真 gap 账本

**状态:v3.8.0 定稿(2026-07-05)**。复测节奏:每个 release 前全矩阵
重跑;对标物升级(valkey / redis-stack 新 GA)= gap 表重测。
裸面 perfgate ratchet 的盒级环境阻塞档案见
PERF-FINDING-2026-07-05-tails-closure.md(floor 不下调,盒恢复复验)。

初版:2026-07-05(v3.3 基线 arena)。对标物版本入账;gap 规则:
差值 ≤ max(两侧 stdev) = NOISE。协议:lx64,隔离(单服务器占同核),
host loopback,client 绑异核,median-of-5 + sample stdev。

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

**判定:非回归,instance 级双峰,观察项关闭。** 同一 binary 三轮
fresh instance 在 2.91M / 3.20M 两个模式间跳——round 2 逐位复现
v3.17.0 的 3.20M,round 1/3 复现 v3.18.0 的 2.91M。两个"版本值"
都是同一 instance 分布的两个峰(page placement / IRQ luck;
legacy8sh SET 双峰同款,见 PERF-FINDING-2026-07-03-legacy8sh-set-
bimodal.md)。v3.17→v3.18 的 -9% 是单轮 arena 各采到一个峰的读数
差,不是代码回归。vs valkey 1.64-1.80× 领先不变。

后续盯法:LPUSH(连同 INCR/SADD/HSET/ZADD)自本轮起进 perfgate
legacy_8sh_* ratchet(K-402)——观察项从人工复测改为每 gate 自动化,
floor = 基线 ×0.92 吸收双峰带宽。

## v4 T4 K-402 — perfgate 扩到 12 角 + baseline 重录;**gate 抓到 v4 SET 写路径真回归**(2026-07-11,lx64)

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

## v3.18.0 release arena — bare face 复测(2026-07-10,lx64)

结构 arc(LOC 还债 + 热路径批 D + fuzz/polish)后的裸面确认:
**7/7 全胜 valkey 9.1**(GET 6.39M = 3.00×,SET 6.38M = 3.99×,
INCR 3.00×,SADD 2.50×,HSET 2.25×,ZADD 1.73×,LPUSH 1.64×)。
GET/SET 与 v3.17.0 基线持平(批 D perfgate ×3 median 已单独验证
热路径拆分零回归)。注:LPUSH 2.91M 较 v3.17.0 的 3.20M 低 9%
(超该格 stdev 4.1%,共享盒单轮;vs valkey 仍 1.64× 领先),
下一 release 复测观察——不构成 ship 阻塞,构成观察项。
