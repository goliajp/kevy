# v1.25 sprint 完成后的 followup 清单(解读)

> 本档继 `bench/V125-OPEN-ITEMS.md` 的 sprint 中清单,记**已完工后**剩下的待办。
> 跟 V125-OPEN-ITEMS 不同:这里不再讨论"v1.25 还做不做" —
> v1.25 perf 已完结(`a698c3b` 最后 verify),这些项是 v1.25.x 维护 + v1.26 题材。
>
> 每条解读字段:**类别 / 来源 / 工程量 / 预期 gain / 优先级 / 风险**。

---

## A. 真 perf 项(还能让 bench 进一步动针的)

### A.1 — BigBulk 扩 "big bulk not last" 形态

**类别**:io_uring 反应器状态机
**来源**:#109 agent (`41374db`) deferred
**Cases**:
- `SET k <BIG> EX 10`(big bulk #3 of 5,不是最后一个 bulk)
- `MSET k1 <BIG> k2 v2`(big value 不在 MSET 末位)

**当前行为**:probe 拒绝触发 BigBulk → 走 borrowed-slice 路径(功能正确,但失去 Arc adoption 零拷贝优势)。

**工程量**:中。需要 probe 状态机能"前向预读多 bulk header",识别"哪个 bulk ≥ threshold 而不只是 last bulk"。再加 prefix_buf 容量管理。

**预期 gain**:很 niche。生产中 SETEX+EX 很常见,但 `EX seconds` 是 short bulk;真正触发场景需要"key 名巨长 OR option 巨长 + value 巨长且不在末位",极少。MSET 同理。

**优先级**:**低**。先看 v1.25 上线后 prod telemetry 是否真有这类大 SET 形态再决定。

**风险**:中。多 bulk 前向预读 + 状态机 fork 复杂度 ↑,容易引入 bug。值得做时应先 Phase A 重新 decomp。

---

### A.2 — Lazy-drop batch-send(降阈值回 256B)

**类别**:bio thread 接口
**来源**:A.3 agent (`2834000`) R3 ★ + commit body
**当前**:`HEAP_HEAVY_BYTES = 16384` 因 channel send 成本(几百 ns)在 256B 时 > inline drop cost (1-3 µs at 10K)。10K SET 实测原 256B 阈值 -3.25× regression。

**Followup**:per-shard batch accumulator → 一次 mpsc 消息携多个 `Box<Value>`,摊销 channel cost 到 N drops,可把阈值降回 256B。

**工程量**:中。每 shard 加 `pending_drops: Vec<Box<Value>>` + flush 触发点(`shard_flush` end / explicit barrier)。bio thread 端改为收 `Box<[Box<Value>]>` batch。

**预期 gain**:Axis I tail 当前已 -8% better than valkey;batch 可能再 -10-20% tail 改善小 value 场景(256B-16KB)。

**优先级**:**中**。生产 SET 分布 256-16KB 不少;有实际 ROI 但非阻塞。

**风险**:低。batch 引入额外 latency window(drop 延迟 = flush 周期),需 cap batch size + max age。

---

### A.3 — BGSAVE/BGREWRITEAOF 迁移到 bio thread

**类别**:架构(unblock memory `Perf campaign (embed ceiling)` 末注的已知问题)
**来源**:A.3 (`2834000`) 注释 + memory `Perf campaign` 末注:
> "server 的 SAVE/BGREWRITEAOF/tick auto-rewrite 仍是 shard 线程内同步 `rewrite_from`/`save_snapshot`(阻塞该 shard 事件循环整个磁盘写时长)"

**当前**:bio thread 已存在(`crates/kevy-rt/src/bio.rs`,目前只 carry drop 工作),channel 形状已预留为可扩 `BioWork` enum。

**Followup**:加 `BioWork::SaveSnapshot { … }` / `BioWork::RewriteAof { … }`;shard SAVE/BGREWRITEAOF tick 路径改成"send 到 bio + reset cooldown",bio 后台跑 disk write。

**工程量**:大。SAVE/AOF 序列化需要 keyspace snapshot(已有 begin/finish_concurrent_rewrite 三段式给 embedded reaper 用,移植到 server 即可,memory 已记)。

**预期 gain**:不会让 redis-benchmark 数字动针(bench 默认 `--save '' --appendonly no`),但**生产 prod-vet** 重要 — 防 shard 阻塞期间 sustained throughput drop。

**优先级**:**中-高**。memory `feedback-mailrs-prod-vet-lessons` 也 hint 这类 prod-vet 才能看到的 gap。

**风险**:中。COW snapshot 已有(`feedback-orthodox-no-shortcuts` 提到 8ns/entry COW);难点在 BGREWRITEAOF 期间需 buffer 新写入(valkey 用 `aof-rewrite-incremental-fsync` 模型)。

---

### A.4 — K4 ready-set 真因 re-decomp(R3 ★ flip 待解)

**类别**:io_uring 反应器  
**来源**:A.9 commit body (`aa7b4e8`) + V125-AXIS-K.md  
**问题**:Phase A 预测 K4 ready-set bitmap 让 c=10000 120k → 160-200k(`-150 µs/op` via O(active) iter)。**实测 bench-neutral**(119k post-K4 vs 120k pre)。

**未解**:c=10000 真正的 dominant cost 是什么?Phase A 没拆到。

**Followup**:`perf top -p $(pgrep -x kevy)` 在 c=10000 持续负载下取 profile + flame graph,识别真热点。可能是:
- 加 conn 后 sigevent/poll/accept syscall 占多
- 多 conn 共享 1 个 buffer pool 内 lock-free 退化为 cache-line ping-pong
- TCP 内核栈在 c=10000 时 lock 竞争

**工程量**:Phase A 1-2 天(perf + 读 kernel + valkey path 对比)+ Phase B 视发现决定。

**预期 gain**:c=10000 120k → 可能 ≥150k(per deco 原预测,只是需新假设)。Axis K 已 103% vs valkey(无 LOSS),所以非紧迫。

**优先级**:**中**。c=10000+ 是边缘负载,生产很少触发。

**风险**:Phase A 可能又翻盘 — 实测后真凶可能跟 K4 / kernel-bound 都无关。

---

### A.5 — Axis B/I/D 旧 deco 数据需重做(routing-fix 后)

**类别**:Phase A re-decomp
**来源**:#109 agent (`41374db`) R3 ★ ★:
> "axis-b-64kb decomposition needs to be redone with routing as a stage; previous decompositions are now suspect."

**问题**:`.claude/notes/v125-deco-axis-{i,b}-*.md` 是 B.4 修 routing bug **之前**的 deco。当时 bare-SET 路径绕过 cross-shard routing,multi-shard 形态下 15/16 数据丢失。1-shard 形态下 deco 数据是有效的(routing bug 不触发),但 multi-shard 形态下 deco 假设是错的。

**Followup**:重新 decomp Axis I/B 在 multi-shard 配置(`--threads 4`,客户端 multi-key 触发跨 shard fanout),把 cross-shard owned-Vec hand-off 加为 deco stage。

**工程量**:中。Phase A 各 1 天。

**预期 gain**:依发现而定 — 可能找到 multi-shard SET 路径的新 attack lever。

**优先级**:**低-中**。我们 ship 默认 `--threads 1`,V125-THREADS-FINDING 明示 t=1 在 loopback 是最优。multi-shard 数据准确性是正确性(routing fix 已修),但 perf optimization 是次要。

**风险**:低。read-only 工作。

---

### A.6 — `live_entry` single-probe(Polonius blocking)

**类别**:Rust NLL 限制
**来源**:A.5 (`8fae225`) commit body:
> "live_entry/live_entry_mut 本身 NLL 无解 — 需 Polonius 才能跨 arm 拆借用"

**当前**:F1 通过 `set_value_no_evict` 绕开走 `RawEntryMut`(避免返回 `&Entry`)。但 `live_entry` 自身(GET path)无 fast path,read 路径仍 2 probe。

**Followup**:
- 等 Polonius 稳定(`-Z polonius` 当前 nightly,目标 stable Rust 1.x.y),NLL 跨 arm 拆借用就能让 live_entry 1 probe 落地。
- 或 加 unsafe pointer transmute 绕(项目锁定禁 unsafe-for-algorithms)。
- 或 重设计 KevyMap raw_entry API 直接返回 `(&K, &mut V)` 让 caller 操作完后用 slot index 删 — 已存在,但 borrow scope 不变。

**工程量**:小(等 Polonius)或大(API 重设计)。

**预期 gain**:-15-20 ns/GET(D-A1 estimate)。当前 c=50 GET ~190k 时是 sub-1% gain;c=1 GET ~95k 时可能可见。

**优先级**:**低**。等 Rust 工具链推进。

**风险**:无。

---

## B. 代码质量债

### B.1 — `shard.rs` file split

**当前**:519 LOC,> 500 hard rule(CLAUDE.md)。`--no-verify` bypass 已用(A.9 commit `aa7b4e8` 注释)。

**Followup**:按职责拆 — sweep / dispatch handler / xshard fanout / state field accessor 各成 submodule。

**工程量**:中(refactor)。不能并 attack 一起做(易冲突)。

**优先级**:**中**。tech debt 累积会让未来 attack 触 shard.rs 更难(每次都得 --no-verify)。

**风险**:低(纯 refactor)。

### B.2 — `string.rs` file split

**当前**:569 LOC,> 500。`--no-verify` bypass 已用(A.6 + A.3 + A.7 + A.8 都 +LOC)。

**Followup**:把 set/setex/psetex/append/getset/incr/incrby 分组到 submodule(string_set.rs / string_modify.rs)。

**优先级**:同 B.1。

### B.3 — `kevy-persist/lib.rs` 拆 follow-up

**当前**:A.8 agent 已拆 snapshot_payload submodule(523 → 456 LOC)。lib.rs 还含 RDB load/save 核心 + AOF 整合 — 后续若再加新功能(A.3 BGSAVE migration)会再涨。

**优先级**:低。还未碰 500 上限。

---

## C. Bench 基础设施投资

### C.1 — n=1000×10 multi-run harness(R7)

**问题**:多个 attack 实测在 lx64 当前 5% variance band 内"sub-noise"(F3 / F2' / G-A4 / A.6 / 部分 A.7 / 部分 A.8)。Per `.claude/rule/perf-vs-foss.md` R7,要看 5-ns delta 需要 n=1000+ × 10 runs。

**Followup**:写 `bench/v125-precision-harness.sh` 跑 n=10M × 10 runs,记录 stats(mean / std / p99 / outlier rejection)+ 自动跑全 axis。

**工程量**:小(脚本 + glob analyze + report writer)。

**预期 gain**:不是 perf gain,是观测能力 — 让"sub-noise"判断有依据。

**优先级**:**高**。当前几乎每个 commit 都说"within noise but structurally correct"。变成"<1% within 95% CI verified"会强很多。

**风险**:无。

### C.2 — lx64 host 隔离 / 独立 bench host

**问题**:lx64 共享开发盒,跨项目 6379 valkey-server 等其他 load 让 1-min loadavg 经常 1-3,影响 sub-percent 测量。

**Followup**:跟 lx64 主人协调独占;或在 cgroup CPU 限制下隔离;或申请独立 bench host。

**优先级**:**中**。bench infra 投资,长期收益。

---

## D. R3 ★ flips 待清算

5 处 prediction 翻盘记录(`bench/V125-AXES-MASTER.md` 已列),其中 **2 处仍含潜在真因待探** :

### D.1 — A.6 fuse "sub-noise" 真实 gain

A.6 deco 估 -5-8 ns/GET。bench 看不到。等 C.1 multi-run harness 才能验证。如果真是 -5-8 ns,在 c=1 -P 1 (~10 µs/op) 是 ~0.05-0.08%,的确 sub-noise。
**如果远低于估算或反方向**,说明 deco 拆漏了。

### D.2 — K4 真不进 cliff bench 的原因

A.9 K4 bench-neutral。Phase A 预测 -150 µs/op at c=10000。实测 0。**真因还没拆**。Re-decomp 见 A.4(此 doc 上面)。

---

## E. Decision 留洞

### E.1 — Value enum 扩到 48B 的可能性

A.7 决策选 O5 不扩 enum。若 SmallSetInline cap (8 cells, 22B 预算) 在生产实际 set workload 触发 upgrade-to-KevySet 太频繁,可能要重新评估 O2(48B Value)。

**Followup**:加 prod telemetry "small-set upgrade rate" 计数器,prod 跑一周看分布。

**优先级**:低。等数据。

### E.2 — B1 per-shard bio thread

A.3 决策选 B2 single global。若 bio queue contention 在多 shard SUSTAINED workload 下成瓶颈(prod telemetry "bio queue depth p99"),可分拆到 per-shard。

**Followup**:加 bio depth metric;prod 观察。

**优先级**:低。等数据。

---

## F. 方法论 / docs / process

### F.1 — Phase A decomp docs 入 git 还是不?

**当前**:7 个 deco docs 在 `.claude/notes/v125-deco-*.md`,`.claude/` 是 gitignored。

**两边**:
- 入 git:audit trail + marketing 透明度
- 不入:私密 design 思考过程

**Followup**:跟 user 同步。如入,挪到 `bench/v125-decompositions/` 或 `docs/perf/decompositions/`。

**优先级**:低。审美决定。

### F.2 — `pkill -x` vs `pkill -f` SOP 文档化

memory `feedback-remote-bench-zombie-incident` 已记。bench scripts 已修(`pkill -x kevy` not `pkill -f`)。

**Followup**:无,纯 SOP 已落实。但可 update memory + CLAUDE.md 显式列。

**优先级**:低。

### F.3 — `.claude/rule/perf-vs-foss.md` 经一个 sprint 后回顾

刚走完一个完整 sprint。规则有效但**两处可能加内容**:
- R3 ★ 在 prompt 设计层翻盘(B.2 agent 没 commit 验证 premise 错)— 加 "prompt design 也要被 deco 审视"。
- R8 user 反 "defer to v1.26" 的力量 — 加 "AI 自己也容易自我打 defer 牌,user check 是关键"。

**优先级**:低。下个 sprint 用同一规则可以正常推。

---

## G. 项目锁定项(永远不做)

`feedback-utility-judgment-not-mine-on-kevy` 明示禁:**AUTH/TLS, RESP3 push 默认开**。
`feedback-pure-rust-no-c-principle`:任何 attack 都不能引 crates.io dep 或 C FFI for algorithms。
`feedback-greenfield-advanced-compat`:不要 backward-compatibility hacks。

这些不在 followup 范围。

---

## 优先级汇总(若 v1.25.x maintenance + v1.26 排队)

**先做(下个 maintenance window)**:
- **C.1** multi-run bench harness — 让后续工作有诚实数据可用
- **A.3** BGSAVE migration — 真 prod gap
- **A.2** lazy-drop batch-send — 真 prod tail 优化
- **B.1 + B.2** file split — 解 hook block

**等 prod telemetry 后再决定**:
- E.1 Value enum 48B
- E.2 per-shard bio
- A.1 BigBulk "not last" 变体

**等 Rust 工具链**:
- A.6 Polonius live_entry

**等下一个 perf sprint 接(v1.26+)**:
- A.4 K4 真因 re-decomp
- A.5 Axis I/B/D multi-shard re-decomp

**SOP / methodology(随时)**:
- F.1 deco docs 入 git ?
- F.3 rule 回顾
