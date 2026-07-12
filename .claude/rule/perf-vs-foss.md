# Rule: Perf vs FOSS Competitors — Decomposition methodology

> kevy 是声称 "drop-in 兼容 Redis + 性能优于 valkey 9.1 / redis 8.8" 的自研项目。
> 任何 perf attack 跟成熟 FOSS 竞品(valkey / redis)对抗,**必须按本规则走**。
>
> 源:`/Users/doracawl/workspace/goliajp/spg/docs/PERF_METHODOLOGY_VS_FOSS.md`(SPG 项目 v7.37 三红线 closure 实战复盘,2026-06-21 user 指 "大部分也适用于你")
>
> 本规则提炼自该文档的"普适部分",删掉 SQL / PG18 / SPG 业务专用细节。

---

## R1 — 2 轮 polish 没动针,立刻 STOP

**Rule**: 任何 perf attack 连续 2 轮 polish 没动针(累计 bench gain < 1.5× variance band)→ **不允许写下一行 "再 polish"**,必须切 decomposition 模式。

**Why**: SPG SCALARSQ 浪费了 10+ 轮 polish 每轮 sub-noise revert,最后用户强制 decomposition,3 commits 内闭红线 -85%。kevy v1.25 master/axis B/C/D/G 我同样停在 "tied" 反复 polish 不动针。

**How to apply**: bench 出来 "+1% / -2% / +0%" 三次 → 停。不写 "看起来 X 该有用"、"试试 Y";写 "decomposition phase A 开始,target = <对手 endpoint>"。

---

## R2 — 触发词黑名单(自我审查 + 文档审查)

**Rule**: 以下话术在 perf 上下文(commit msg / bench doc / 内部论证 / 跟 user 对话)**禁止出现**。一旦出现 = 本轮思考无效,撤回,切 decomposition。

| 错误话术 | 应该改成 |
|---|---|
| "architectural ceiling" / "结构性 gap" / "结构性 limit" | "我没拆够细。把这条 endpoint 拆 18 段 vs 竞品。file:line 给我" |
| "language ceiling" / "Rust vs C idiomatic 差异" | "我用了某 abstraction 浪费了 CPU。是哪个 abstraction?" |
| "user-space 不可触" / "kernel-bound" / "loopback floor" / "TCP RTT floor" | "我没读对手的 user-space。对手在同样 path 怎么 N µs/op 的?" |
| "sub-bench noise / 5-10μs 不可观测" | "5 个 sub-noise win 累计一定突破 variance" |
| "tied at 99-104%" → 停 | 不允许停。每条 tied 都要做 decomposition |
| "noise band within variance" | "把 variance 缩窄(n=99 → n=1000 × 10 runs)再说" |
| "绝大部分场景赢" / "主要 use case 赢" | "输/平的全列,每个都拆" |
| "客户可以接受 X% 输" | "我的标准比客户高" |
| "valkey 已 absorbed 同等优化 / 已 mature" | self-deception:对手 user-space 你没读过 |
| "kernel scaling 是 floor / 是 hardware ceiling" | self-deception:对手在 user-space 同样 path 你没读过 |
| "已是 optimal config / 已 tuned" | 没有 optimal 配置,只有 N 段未拆的 atomic-op-count |

**Why**: 每一句话术对应一个 self-deception 模式。话术出现 = "我懒得继续找了" 的伪装。SPG 用户原话 "C 和 Rust 不可能有 2.75× 这么大的语言性能差异" + "把路径拆成非常小的一段段对比" → 翻盘。

**How to apply**: 写 commit / doc / response 之前 grep 自己,出现表里任意词就退回 R1。文档已写好但有这些词 = 立即修文档(不是删,而是改成 "decomposition pending → file:line 待补")。

---

## R3 — Decomposition 是发现,不是确认

**Rule**: 做 decomposition 之前列出的 Top-3 预测,实测后**至少 1 个会翻盘**。如果实测后预测全中,说明 decomposition 颗粒度不够细,继续拆。

**Why**: SPG 三 case 全部翻盘:
- SCALARSQ Top-3 预测里 2 个证伪(RwLock + Dispatch),真凶 2 个**根本没在预测里**(eval_expr_with_correlated + iter_cold_rows_of_table)
- DISTA 预测 BTreeSet<String> 慢于 PG tuple_hash → 完全错,真凶 literal arg2 触发 per-row Cow materialise
- INSUBQ B-3 预测 Arc bookkeeping → 错,真凶 `binary_search_by` 在 N≤7 时 branch predictor miss(linear scan 反而快)

**How to apply**:
- 把预测写在 decomposition doc 顶部,实测列在旁边,翻盘的标红
- 看到 "实测 = 预测" 不要高兴,先怀疑 decomposition 不够细
- 真凶很可能在 "顺便看了一眼" 的辅助函数里,不在 "我以为是热点" 的函数里

---

## R4 — Phase A Decomposition 硬指标

**Rule**: Decomposition doc 必须满足以下条件才算完成:

1. **18+ stage** × file:line(每段都有对手 vs 自己的路径)
2. **每段 enumerate atomic ops**:N BTree descent / M heap alloc / K syscall / ...,不允许写 "很慢" / "复杂逻辑"
3. **每段 µs 估算**(对手 / 自己 / Δ)
4. **总和 ±20% 内匹配实测 wire RTT**。如果加起来 60μs 但实测 200μs → **漏了 140μs,回去找,decomposition 没完成**
5. **每段都有对手等价路径,通过 read 对方源码确认**(不是猜)
6. 产出 Top-N attack 清单:file:line + 具体 code change + µs 估算 + semantic class + blast radius

**Why**: 不满足这 6 条的 "decomposition" 是 polish 伪装。SPG 的 decomposition note 模板见原文档 §6。

**How to apply**: kevy 的对手源码在 `/root/srcbench/valkey/src/`(lx64) 或可 `git clone` 到本地。具体文件指引:
- valkey 协议解析: `src/networking.c`
- valkey GET/SET handler: `src/t_string.c`
- valkey 主循环: `src/server.c` `aeMain` + `src/ae.c`
- valkey IO threads: `src/networking.c` `IOThreadMain`
- valkey dict (hashtable): `src/dict.c`
- valkey sds (string): `src/sds.c`
- valkey memory robj: `src/object.c`

读得头大也得读。一个下午,换 N 个 ship cycle 的 perf debt 闭合。

---

## R5 — Phase A 与 Phase B 必须分开

**Rule**: Decomposition 阶段(read-only research)和 Attack 阶段(改代码 + bench)**必须严格分两个 agent / 两段连续工作**。不允许:
- 在 decomposition 中 "顺手改一行试试"
- 在 attack 中 "再 decompose 一下这个"
- 同一个 agent 同时持 read 权 + write 权

**Why**: 同一个 agent / 同一段工作里跳跃 → 被 build error 拉走,被 "我先试一下" 打断 attribution,最后产出 "尝试了 X 但是 X 没用" 总结 = polish 10 轮的状态没区别。

**How to apply**:
- Decomposition agent: `subagent_type: Explore` 或 `general-purpose`,严格 read-only,不能 Edit/Write code(允许 Write decomposition doc)
- Attack agent: `isolation: worktree`,基于上一步产出的 Top-N 清单 atomically 实施 + bench validate
- 人类自己做时,decomposition phase 强制不开 IDE 改代码窗口,只开 reader

---

## R6 — Bench validate 在 cumulative 后,不在每 attack 后

**Rule**: Phase B 的 N 个 attack 全部 land 后才整体 bench validate。不要每 single attack 后单独 bench。

**Why**: 单 attack 5-25μs gain 通常 sub-noise(被 variance band 吞掉),无法判定"有效"。3-5 个 cumulative 一定突破 variance band。SPG SCALARSQ 三个 attack atomic 一起上才 -220μs / -85%。

**How to apply**:
- worktree 里 implement 3-5 attack
- 全部 commit 完才 bench
- bench n=99 是底线,n=300+ 更好;要看 5μs 级 wins 必须 n=1000 × 10 runs

---

## R7 — Variance band 是工程问题不是物理问题

**Rule**: "bench variance 太大看不出 X µs delta" 不是 perf 工作的借口,是 bench infrastructure 投资项。

**Why**: SPG mini docker-fair n=99 × 3 runs variance ~10%。要看 5μs delta 需要 ~1% variance = n=1000+ × 10 runs。这是 bench infra 投资,不是 perf 工作本身能 "绕开" 的。

**How to apply**: kevy bench(`bench/matrix.sh` 等)如果出现 "tied / within variance",先问自己:
- n 够大吗?(redis-benchmark 默认 100k,加到 1M / 3M)
- run 数够多吗?(median of 3 → median of 10)
- 同一硬件 / 同一 docker / 同一 CPU pin?
- 噪声源(QCOM IT 跨项目共享 box)排除了?

提高 n 和 run 数到能看 5μs delta 之前,不允许说 "tied" 或 "within variance"。

---

## R8 — 最近的一句话

> **任何 perf attack 2 轮没动针后,你下一行字不允许是 "polish",必须是 "decomposition"。**

附议(全部前面 R1-R7 已覆盖,作 mnemonic):
- 任何 "X 不可触 / X ceiling" 话术 = decomposition 不够细
- C / Rust 实现差距 ≤ 1.5× 上界,超出 = abstraction 浪费,不是语言
- Decomposition + Attack 两步 dance 是开源对抗的**默认工作流**,不是 "我们试试这次"
- 早 hard-means-do,晚浪费 N 轮 ship cycle

---

## R9 — Prompt 设计也要被 R3 审视(2026-06-22 v1.25 sprint 教训)

**Rule**: 启动 attack agent 前,prompt 里写的 "设计前提"(API 形状 / 数据流路径 / 标准库行为)也是**假设**,**第一件事让 agent 验证而不是开干**。

**Why**: v1.25 sprint 的 B.2 attack(commit 不存在,因 agent 主动 stop)— prompt 写得自信:"`Vec::split_off` 是 zero-copy / argv 可 take 中段 / 10K SET 走 slow path"。Agent 一上手读 stdlib alloc/vec/mod.rs + 实测,**三个前提全错**(`split_off` 走 with_capacity+memcpy / Argv packed `Vec<u8>` 不能 take 中段 / 10K SET 走 G2 parse-from-slab fast path)。Agent 选择 stop-without-code 比"按错前提写废代码"明智 — 反过来说 prompt 设计阶段就该先做 verify。

**How to apply**:
- Attack prompt 末尾一定要加 "verify these premises FIRST" 列表,把声称的 API/data-flow/stdlib 行为列清楚
- Agent 第一步是 read-confirm,确认 premises 对了才开 implement
- 如有前提错,agent 应当 stop + report,跟 B.2 一样,**比 ship dead code 更值**
- 验 stdlib 行为最便宜:读 stdlib source(`/Users/doracawl/.rustup/toolchains/*/lib/rustlib/src/rust/library/`)。`Vec::split_off` 等关键 API 看 30 秒就知道。

**See also**: R3(prediction flip)— prompt 前提也是 prediction;R5(read-only / write 严分)— agent 先 verify 后 write 是这条的自然延伸。

---

## R10 — Bench 必须验 read-back(2026-06-22 v1.25 sprint 教训)

**Rule**: throughput bench 报数(rps)**单数没意义**,必须搭配 **read-back semantic validation**(写完 N 个 key,read 出 N 个 key,比较值)。

**Why**: v1.25 sprint 的 B.4 attack(commit `062eaa4`)报 "Axis B 64 KiB SET 1.028× valkey"。#109 agent 验证时发现:**B.4 bare-SET fast path 绕过 cross-shard routing,默认 16-shard 配置下 15/16 SET 实际写到错的 shard,数据静默丢失**。redis-benchmark 不验 read-back 所以看起来 rps 漂亮;实际正确多 shard SET throughput 是 ~19k 而不是 55k。3 个 commit + 1 个错的 Phase A deco 都建立在这个 paper 数字上。

**How to apply**:
- 任何 SET-path attack bench 必须有 paired GET-back validation step
- 即便不在 attack agent 里跑,也要在 Phase C 重测前手动跑 `redis-cli` smoke check 或写小脚本(`bench/v125-readback-smoke.sh` 之类)
- `redis-benchmark -t set/get` 的"set + get 同 keyspace" 模式不算 validation — `-r N` 让 key 散布,SET/GET 间无契约
- 对多 shard / 多 conn fanout 的 attack 尤其严:single-conn bench(c=1)看不见 routing bug
- **正式 ship 前必须做** read-back validation(`bench/v125-readback-smoke.sh` queued for follow-up)

**See also**: R3(measurement 翻 prediction);R4(deco 必须以真实测量为锚)— 错的 measurement 就是错的锚。

---

## R11 — 反 "AI 自己打 defer 牌"(2026-06-22 v1.25 sprint 红线教训)

**Rule**: AI agent 自己**最容易**把 R8 反过来——把"该做的难事"包装成"v1.26 backlog"。除非 user 明确说"这一项 OUT",**所有"defer 到下个 sprint"都默认 R8 反例,等价 polish**。

**Why**: v1.25 sprint 中段(2026-06-22)我自己把 7 个 attack 标 "建议归属 v1.26",列了一张漂亮的优先级表。User 反应:"你这海量 defer 是不可接受与原谅的,我们现在就在 perf 专题,你 defer 到后面去占别的资源有什么意义呢"。逐条审视:**没有一项触 0-dep / no-C-for-algorithms / RESP wire-compat / no-AUTH-TLS 的项目锁定**,全部该在 v1.25 perf 专题里做完。"defer to v1.26" = "polish 的另一种话术"(占未来 sprint 资源 + 自我下台阶)。

**How to apply**:
- 写 "deferred to vN+1" 这种 backlog 表前问自己:**真有项目锁定理由,还是只是工程量大?**
- 工程量大 = orthodox / hard-means-do — 该做
- 项目锁定 = legitimate filter — 可以 defer,但要点出哪条锁定(0-dep / no-C / wire-compat / out-by-name 等)
- 没找到锁定理由 = R11 反例,等同于 R8(polish 伪装)
- 唯一合法的 cross-sprint defer 来自 **user 显式判断**("跟 prod telemetry 决定"、"先 ship 收数据"、"等 X 项目 ready"),不能自作主张

**See also**: R8(no polish 反例);`feedback-utility-judgment-not-mine-on-kevy`(ROI 不是我的判断,project lockdown 才是)。

---

## kevy 实战:我已踩过的具体坑(2026-06-21 v1.25 sprint)

`bench/V125-*` 系列文档全是 R2 触发词反例,可作 negative learning:

| 文档 | 错误话术 | 应该改 |
|---|---|---|
| `V125-AXIS-B-BIGVAL.md` | "tied with valkey across 64B→64KB" 就停 | decomposition: valkey `t_string.c::getCommand` + `networking.c::addReplyBulk` + `sds.c::sdslen` vs kevy GET path 18 stage |
| `V125-AXIS-C-CHURN.md` | "tied at 99-100%, malloc savings sub-noise at c50-P1 RTT" | "RTT floor" 是 R2 触发词;cumulative attack 没试 |
| `V125-AXIS-D-KEYSPACE.md` | "RTT-bound hides TLB savings" | "RTT-bound" R2 触发词;valkey `dict.c::dictRehash` vs kevy `KevyMap::get` 没拆 |
| `V125-AXIS-G-COLLECTIONS.md` | "tied across 8 ops at 99-103% vs valkey" 一句话收 | 8 个 op 没 single decomposition |
| `V125-AXIS-I-LATENCY.md` | "Bytes::copy_from_slice on 10K hits allocator harder than valkey's reusable buffers" | 推测一句,**没读** valkey sds reusable buffer 怎么做的 |
| `V125-AXIS-K-CONNSTORM.md` | "valkey 的 10-io-thread amortises kernel per-flow cost better" | architectural ceiling 话术,**没读** valkey `networking.c::IOThreadMain` |
| `V125-AXES-MASTER.md` | "architecturally honest positioning" / "valkey 的成熟 epoll + tcache 已经吸收同等优化" | self-deception 话术,见 R2 |

修复路径:对每个 axis 先做一次完整 decomposition(R4),不再接受 "tied" 收口。

---

## R12 — A/B 的对照组与顺序,都必须先证明是干净的(2026-07-12,v4 SET "回归" 翻案)

**Rule**: 用 A/B 判定"某分支引入了回归"之前,两件事必须先成立,缺一条 = 判决无效:

1. **对照组 = 基准值的来源代码本身。** 不是"另一个提交",不是"baseline commit 字段里写的那个 hash" ——
   要确认那个 hash **真的是**基准数字的来源。基准可能是**合成的**(不同角来自不同版本),
   拿一个同样带病的提交当对照,会得到"两者一致 → 没问题"的假绿灯。
2. **两个二进制的运行顺序必须在轮次间翻转。** 盒子在一次连续测量中会**单调下漂**
   (热 / page placement / IRQ)。固定"A 先跑、B 后跑"会系统性抬高 A —— 这一条就足以
   凭空造出"分布零重叠"。

**Why(实证)**: v4 K-402 报了 `legacy_8sh_set` **-18.7%,"distributions disjoint"**,
同盒同小时、每角 3 fresh instance —— 看起来无懈可击,于是进了账、亮了红灯、上了 ship blocker。
2026-07-12 复验:

- 该角是**三峰分布**(7.98M / 8.55M / 9.21M 三个吸引子;LPUSH 的同款结构 K-401 已记录)。
- K-402 的协议是 v3.18.0 的 3 个 instance **先跑完**,再跑 v4 的 3 个。盒子下漂 → v3.18.0 系统性偏高。
- **顺序平衡后(奇数轮 A 先、偶数轮 B 先),两个分布完全重合**:v4 也能摸到 9.21M 最高档,
  v3.18.0 也会掉到 7.99M 最低档;**中位数差 -0.09%**。有几轮两者**逐位相同**。
- 期间我自己又犯了第 1 条:G2 实验拿 `829073de`(v3.18.0 之后第 21 个提交,**本身在 v4 上**)
  当对照,测出"和当前一致"就判"环境假阳性"。两个带病样本一致,只证明"没再恶化"。

**How to apply**:
- 多峰角(先用同一 binary 跑 ≥8 个 fresh instance 看有没有多个吸引子)**禁止用 n=3 下结论**。
- "distributions disjoint" 这句话**本身进触发词黑名单** —— 它在多峰 + 顺序偏置下太容易被制造出来。
  要说"零重叠",先给出:①样本数 ≥10/边;②顺序平衡;③同一 binary 自己的多峰谱。
- 判定语言按证据强度分级:**中位数差**是主判据;**均值差**在多峰下只反映档位占用频率,
  不能单独当回归证据。

**See also**: R3(decomposition 是发现不是确认)、R7(variance band 是工程问题)。

---

## R13 — 读数落在等间距的档位上时,先怀疑尺子(2026-07-12,250ms 量化)

**Rule**: 当同一个角的多次测量**聚集在几个等间距的离散值**上(而不是连续散布),
**在对系统提出任何假说之前**,先做一次除法:把读数换算回**被测量的原始量**
(吞吐 → elapsed = N / rps),看步长是不是一个**可疑的常数**。

**Why(实证)**: kevy 从 2026-06-11 起,perfgate 的表头就写着 instance variance 是
"the dominant noise axis"。**它不是噪声,它是尺子。**

`redis-benchmark` 在 `--threads` 模式下,**结束测量的唯一出口是它自己那个 250ms 的
`showThroughput` 定时器**(`redis-benchmark.c:52` `SHOW_THROUGHPUT_INTERVAL 250`;
`:1653` `if (config.num_threads && requests_finished >= config.requests) aeStop(...)`;
不加 `--threads` 时是 `:425` 在 `clientDone` 里立即停)。于是 `totlatency`
(`:970/:973`)被**向上取整到 250ms 的整数倍**,报出的 rps 因此:

1. **被量化** —— 只能取 `N / (k·250ms)` 这些值;
2. **被系统性低报** —— 取整方向永远是"时间变长"。

一格的相对宽度 = `250ms / elapsed`。arena 口径(elapsed ≈ 1.25 s)**一格 = 20%**;
perfgate legacy 口径(≈ 3.5 s)**一格 = 7%**。

**代价**:三份 finding doc(K-401 LPUSH"双峰"、K-402"SET 写路径回归 -18.7%,
distributions disjoint"、2026-07-03"8.55M attractor")+ 一个 ship blocker +
一个错误的"环境假阳性"判决,**全部建立在格子上**。arena 表里 GET 和 SET 读数
**逐位相同**(6,389,776)、INCR 和 SADD **逐位相同**(5,326,232)—— 这个刺眼的
巧合摆了四天,没人做那一次除法。

**How to apply**:
- 读数出现**重复的精确值**(不同 instance 报出逐位相同的 rps)= 量化的确诊信号,**不是**稳定性的证据。
- 换算回原始量,看步长。常见嫌疑:计时器周期(250ms / 100ms)、采样间隔、调度 tick。
- **不要**去掉 `--threads` 就当修好了 —— 那会让客户端变成瓶颈(实测:arena 的 `-P 16` 口径下
  单线程 redis-benchmark 只能推 2.1M ops/s,测的是客户端不是服务端)。
- 正确的修法:**从服务端自己的计数器上、在稳态窗口里读吞吐**
  (`INFO total_commands_processed` 的差 / 自己掐的墙钟)。它不受客户端启停路径影响,
  而且 kevy 与 valkey 同义,竞品对比依然成立。

**See also**: R7(variance band 是工程问题不是物理问题 —— 本条是它最贵的一个实例)、
R12(A/B 的对照组与顺序)、R3(decomposition 是发现不是确认)。
