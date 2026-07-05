# PERF-LEDGER — kevy vs FOSS 真 gap 账本

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
