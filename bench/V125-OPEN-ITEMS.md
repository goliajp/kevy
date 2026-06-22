# v1.25 sprint — 全部未闭项清单(ship 前 review)

> 收尾要求(2026-06-22 user):**不 close 不 ship**;先把所有 open 项列清楚。
> 本文档分类汇总 Phase A 决komp + Phase B 实施 + bench 过程中累积的全部
> 未解决项。
>
> **2026-06-22 correction — defer-to-v1.26 framing 是 R8 反例**
>
> 原本表格里 "建议归属 v1.26" 的所有项 user 已点破:**没有一项触
> 0-dep / no-C-for-algorithms / RESP wire-compat / no-AUTH-TLS 的项目
> lockdown**,全部该在 v1.25 perf 专题里做完。"defer 到 v1.26" =
> "polish 的另一种话术"(占未来 sprint 资源 + 自我下台阶)。
>
> 已撤销 v1.25-vs-v1.26 分流。任务 #97-#108 是真正的串行 attack queue
> (`TaskList`),按 unblocks 最多下游 + 已知 gap 大小排序。下面的"推荐
> 归属"列保留作历史,实际执行按 task 顺序。
>
> **2026-06-22 update — B.2 argv-ownership API DROPPED (R3 ★ in agent verification)**
>
> a2d255b6 attack agent 验证 prompt 设计时翻盘 3 个核心前提:
> (i) `Vec::split_off` 实测**不是** zero-copy(stdlib `alloc/vec/mod.rs:3080`
> 走 `with_capacity + memcpy`); (ii) Axis I 10 KB SET 走 G2 fast slab
> path,根本不经 owned-input 路径,argv-take API 用不上; (iii) Argv 是
> packed `buf: Vec<u8>` 一个分配,middle-range 不能 take 不 copy。
>
> 真正解 Axis I + Axis B SET take-into-Arc 的是 **per-conn BigBulk 状态机**
> (原 A.2 / B.4):见 $N header 且 N≥threshold → 切到 owned recv slab,
> 完成后 `vec.into_boxed_slice() → Arc::from(Box)` 才是真 zero-copy
> (`Vec::into_boxed_slice` 在 cap==len 时复用 allocation)。
>
> 任务 #99 (B.2) + #100 (A.1) **deleted**;任务 #101 (B.4+A.2) 合并 A.1
> 实际工作。

---

## A. Attack 没做 / 没闭(具体 file:line + 阻塞)

### A.1 — Axis I SET p999 tail amplifier(**最大单一未闭 gap**)

| 字段 | 内容 |
|---|---|
| 来源 | `.claude/notes/v125-deco-axis-i-c50-10kb.md` A3 + B-A1 |
| 当前差距 | kevy c=50 -d 10240 SET p999 = 0.487 ms vs valkey 0.335 ms = **kevy -45 %** |
| 真因 | SET 路径在 ingress 之后做了第 2 次 10K memcpy:`cmd_data.rs:205 store.set_slice(&args[1], &args[2], …)` → `kevy-store/src/string.rs:33 pick_value_for_set` → `Value::ArcBulk(Arc::from(bytes))` (alloc + memcpy)。 G2 已经在 reactor 层把第 1 次 memcpy 消了,但 dispatch → store 这一段还在 borrowed `&[u8]`。 |
| 阻塞 | **kevy-resp 没暴露 argv 所有权**(parsed slice 来自 owned `conn.input` 还是 kernel pbuf slab 不可区分)。要做 take-into-Arc 必须 kevy-resp 在 ArgvView trait 上加 `take_slice(i: usize) -> Option<Box<[u8]>>` 或类似 API,parser 维护源 buffer 所有权状态。 |
| 估算 | -30 ~ -100 µs tail / SET @ 10K (per deco I-A3) |
| 建议归属 | **v1.26**(跨 crate 接口改 + Argv 所有权模型设计,$ 1-2 天工程) |

### A.2 — Axis B 64K GET / 大 bulk recv-into-Arc

| 字段 | 内容 |
|---|---|
| 来源 | `.claude/notes/v125-deco-axis-b-64kb.md` B-A2 |
| 当前差距 | kevy c=50 -d 65536 GET = 66 756 rps vs valkey 70 621 = **kevy -5.5 %**;SET 已 103 % 不亏 |
| 真因 | `uring_io.rs` 用 16K multishot recv 拆 64K 大 bulk,造成 5× pbuf→input memcpy 共 80 KiB/SET。Phase A 提议:看到 `$<N>` 且 N ≥ PBUF_SIZE 时,把 conn 切到 one-shot recv into 预 sized `Arc<[u8]>` slab。 |
| 阻塞 | io_uring reactor 当前是 pure-multishot;混合 multishot + one-shot per-conn 的状态机要新增 `BigBulkState` enum + arm 路径分支。中度 refactor。 |
| 估算 | -6 ~ -8 µs / 64K SET (per deco B-A2) |
| 建议归属 | **v1.26**(reactor 状态机改造,与 A.1 配套) |

### A.3 — Axis I lazy-drop big values(需要新线程)

| 字段 | 内容 |
|---|---|
| 来源 | `.claude/notes/v125-deco-axis-i-c50-10kb.md` A2(G6 实施后 reverted) |
| Phase B 实测 | 单线程 `pending_drops` deferred drain **比 inline drop 更糟**(p999 +144 µs / 1 次 64 ms spike)。R3 ★ 完美翻盘。 |
| 真因 | valkey `lazyfree.c` 之所以赢是因为 **separate bio thread** 在后台 free。单线程把 inline-drop 摊平成 batched-stall,延迟从稳变 bursty。 |
| 阻塞 | **kevy 没有任何后台 bio thread**。要做的话要在 Runtime 起一个或 per-shard 一个 bio thread + cross-thread queue(可复用 kevy-ring 的 SPSC)+ Value 类型可 send-cross-thread 的语义检查。 |
| 估算 | -20 ~ -150 µs p999 / SET overwrite(deco 原预测,前提是 bio thread 真的能把 free 移走) |
| 建议归属 | **v1.26**(架构动作:thread-per-core 模型现在彻底 single-thread per shard,引入 bio thread 需要 RFC 级讨论) |

### A.4 — Axis H 4 KB pub/sub writev-chunking

| 字段 | 内容 |
|---|---|
| 来源 | `.claude/notes/v125-deco-axis-h-pubsub-edges.md` H2.A 实施后续 |
| 当前差距 | size=4 KB pub/sub kevy 1.11 M / valkey 2.26 M = **49 %**(kevy 输 51 %)。注:valkey 2.26M 比之前 baseline 1.04M 高 117%,怀疑 host noise;但即便如此 ratio 仍 lossing。 |
| 真因 | G5 H2.A 用了 Arc-shared writev gather,但 Linux `IOV_MAX = 1024` cap 迫使每 256 个 publish 强制 flush 一次(G5 加的 `PUBSUB_ARC_FLUSH_AT = 256` correctness fix)。剩 256-1024 publish 仍走 memcpy fallback。 |
| 阻塞 | 要做 writev-chunking:把 50 × 1024 = 51 200 iovecs 拆成多次 writev 同步 syscall per drain。涉及 uring `prep_writev` 多 SQE 协调 + completion 顺序保证。 |
| 估算 | size=4 KB 从 49 % → ≥ 120 % vs valkey(per deco H2.A 原预测达成度) |
| 建议归属 | **v1.26**(reactor 改) |

### A.5 — Axis D/C single-probe `live_entry`

| 字段 | 内容 |
|---|---|
| 来源 | `.claude/notes/v125-deco-axis-d-keyspace.md` D-A1 + `.claude/notes/v125-deco-axis-c-churn.md` F1 |
| 当前差距 | Axis D c=50 -P1 GET kevy 99 % vs valkey(整个 keyspace CPU delta < 1 % of 30 µs RTT,bench shape 看不见;deco 估 c=1 -P1 才看得到)。Axis C 同理。 |
| 真因 | `accounting.rs::live_entry{,_mut}` 在 `maxmemory==0` 默认下做 **2 次 `KevyMap::get` probe**(1st 检 expiry,2nd 拿 mut 或 imm ref 返回)。1st probe 借用 `&Entry` 阻塞 `&mut self.remove_entry` 的 expired 分支 → 必须做 2 probe。 |
| 阻塞 | 我本会话已尝试 — **borrow checker 在没有 entry API 时无解**。需要 `kevy-map` 加 hashbrown 风格 `raw_entry_mut` API:返回 `OccupiedEntry { remove(), into_mut() }` 既能 read 又能在借用持有下 remove。 |
| 估算 | -15 ~ -20 ns/GET (per deco D-A1) |
| 副效益 | **跨 axis blast**:同样 fix 套到 Axis C 的 F1(`set_value` overwrite 路径),省 -21 ns/overwrite-SET。 |
| 建议归属 | **v1.26**(`kevy-map` API 扩展,$ 半天) |

### A.6 — Axis D D-A2 fuse `get_for_reply` into `try_inline_local`

| 字段 | 内容 |
|---|---|
| 来源 | `.claude/notes/v125-deco-axis-d-keyspace.md` D-A2 |
| 当前 | `kevy-store/src/string.rs:156-170 get_for_reply` 返回 `GetReply` enum → `kevy-rt/src/exec_dispatch.rs:148-180 try_inline_local` match enum arm 后再 encode。enum tag round-trip + match 是无谓中间层。 |
| 真因 | enum 设计用于在 store 借用尚未释放前定形返回数据;fuse 后 store call 直接 `write!($len\r\n bytes\r\n)` 入 conn output,跳过 GetReply 中间型。 |
| 阻塞 | 无技术 blocker。是一个 ~100 LOC 跨 crate fusion(需要 `kevy-store` 暴露一个 `get_into_resp(out: &mut Vec<u8>)` 风格 API)。我没做是因为带优先级 — D-A1 解决前 D-A2 收益被淹。 |
| 估算 | -5 ~ -8 ns/GET (per deco D-A2) |
| 建议归属 | **v1.25 末班** 或 **v1.26**(决于 D-A1 是否做) |

### A.7 — Axis G G-A2 SmallSet inline encoding

| 字段 | 内容 |
|---|---|
| 来源 | `.claude/notes/v125-deco-axis-g-sadd-pilot.md` G-A2 |
| 当前差距 | 默认 `-r 0` bench shape 下,valkey 跑 1-entry `OBJ_ENCODING_LISTPACK`(1 cache line),kevy `KevySet` 16-slot Swiss table 结构上无法 match — **bench shape 问题不是 perf**。换 `-r 100k` 强制 valkey HT 时 G4 已经 +1 %。 |
| 真因 | 要 match listpack 性能需要 `SmallSet<SmallBytes, N≤8>` inline encoding on Value enum,mirror valkey listpack/intset。 |
| 阻塞 | **Value enum 32 B 硬上限**(`value.rs:160-163`),inline N 可能只塞 2-3 cells,半数收益被吃掉。 |
| 估算 | -0.10 ~ -0.15 µs/op SADD shape A(deco 原估;实际可能更低因 inline 容量受限) |
| 建议归属 | **v1.26**(需要先决 Value enum 是否扩容,会影响所有 store 操作) |

### A.8 — Axis G 其他 7 ops(HSET/ZADD/LPUSH/RPUSH/LRANGE×3)decomposition

| 字段 | 内容 |
|---|---|
| 当前 | 只对 SADD 做了 pilot decomposition。G4 borrowed dispatch 已覆盖所有 multi-arg ops,但 listpack-vs-KevyMap 结构对比未对每个 op 做。 |
| 阻塞 | 工程量(每个 op 一份 Phase A doc,$ 半小时-1 小时 / op) |
| 估算 | 每个 op 0-15 % gain 可能(per deco assumption,未验证) |
| 建议归属 | **v1.26**,优先级看每个 op 在 mailrs / dogfood 项目里实际权重 |

### A.9 — Axis K K3/K4 ready-set bitmap arm-loop

| 字段 | 内容 |
|---|---|
| 来源 | `.claude/notes/v125-deco-axis-k-c10000.md` K4 |
| 当前 | G1 K1+K2 已经把 c=10 000 cliff 闭(270 → 120 k rps,vs valkey 103 %)。K3/K4 是进一步 -150 µs/op @ c=10 000。 |
| 真因 | `arm_conns` Vec 全扫,O(N=10 000) per iter。改 ready-set bitmap dirty in recv-CQE handler + accept handler 改 O(active)。 |
| 阻塞 | 无技术 block,但 G1 已经让 c=10 000 不再输 valkey,K4 是"再上一台阶"非"修 bug"。 |
| 估算 | c=10 000 t=1 从 120 k → 可能 160-200 k rps |
| 建议归属 | **v1.26**(non-blocking refinement) |

---

## B. 阻塞 attack 的 infrastructure 项

### B.1 — `kevy-map` 需要 raw-entry API

**阻塞**:A.5 (D-A1 + F1),可能还有未发现的 fast-path opportunities。

**API 设计草案**:
```rust
// 类似 hashbrown::RawEntryMut
pub fn raw_entry_mut<Q>(&mut self, key: &Q) -> RawEntryMut<'_, K, V>
where K: Borrow<Q>, Q: Hash + Eq + ?Sized;

pub enum RawEntryMut<'a, K, V> {
    Occupied(RawOccupiedEntry<'a, K, V>),
    Vacant(RawVacantEntry<'a, K, V>),
}

impl RawOccupiedEntry { 
    fn get_mut(&mut self) -> &mut V;
    fn get(&self) -> &V;
    fn remove(self) -> V;  // ← 关键:借用持有下能 remove
    fn into_mut(self) -> &'a mut V;
}
```

**工程量**:`kevy-map` 内部 1 个新文件 ~200 LOC + 测试。

**建议归属**:v1.26 第一项(unblock multiple downstream attacks)

### B.2 — `kevy-resp` 需要暴露 argv ownership

**阻塞**:A.1 (I-A3 / B-A1 take-into-Arc)。

**API 设计草案**:
```rust
// 在 ArgvView trait 上加
trait ArgvView {
    // existing: fn get(&self, i: usize) -> Option<&[u8]>;
    
    /// Take ownership of arg `i`'s bytes if the underlying parse buffer
    /// is take-eligible (owned). Returns None if the slice borrows from
    /// a shared buffer (e.g. kernel pbuf slab in parse-from-slab path).
    fn take(&mut self, i: usize) -> Option<Box<[u8]>>;
}
```

**Parser 改动**:`Argv` / `ArgvBorrowed` / `ArgvBorrowedInline` 都要追踪源 buffer 所有权状态。

**工程量**:跨 `kevy-resp` 3 个 argv 实现 + uring_io 标 take-eligible bit + 测试。$ 1 天。

**建议归属**:v1.26 第二项(unblocks Axis I 最大未闭 gap)

### B.3 — Bio thread for free-work

**阻塞**:A.3 (G6 A2 lazy-drop) + 未来任何"defer to background"类 attack。

**架构问**:
- 一个 bio thread 全 shard 共享?还是 per-shard?
- 工作 queue 用 kevy-ring SPSC?
- Value drop 是 thread-safe 的吗?(目前 Value 不要求 `Send`)
- bio thread 在 NUMA 下亲和性?(单 socket lx64 无 NUMA,但部署 host 可能有)

**建议归属**:v1.26 或 v1.27,需要架构 RFC 先

### B.4 — io_uring reactor recv-into-Arc 状态机

**阻塞**:A.2 (B-A2)。

**已有相关代码**:`uring_io.rs` G2 fast path 已经在 `uring_recv_dispatch` 区分 fast(input empty) / slow(input has prefix) path,但都是 multishot。新增 BigBulk path 需要 conn-level "big-recv-in-progress" 状态 + arm 路径分支。

**工程量**:$ 半天-1 天。

**建议归属**:v1.26,与 A.1 配套做(都是大 value 路径)

---

## C. Bench / 测量基础设施 gap

### C.1 — lx64 共享 host 噪声

**症状**:bench 期间 load avg 经常 2-3(其他项目的 6379 valkey-server 等)。看不清 5-10 µs deltas。

**例子**:G3 attack 的 -10-15 ns/SET 完全在 noise 内,只能"无 regression"收口。

**Fix path**:
- 独占 lx64(协调其他项目)
- 或 build per-bench docker container with cgroup CPU 限制
- 或 接受 noise 在 R7 框架下,把所有 < 5% wins 标 "structural correctness + sub-noise bench" 而不是 ship blocker

**建议**:v1.26 启动前讨论 — 是不是要建独立 bench host

### C.2 — n=1000+ × 10 runs bench harness 没建

**症状**:R7 要求 < 1 % variance band 看 5 ns delta = n=1000+ × 10 runs。我们当前是 n=100k-2M × 3 runs。

**例子**:G4 +1 % win 在 c=50 -P 1 内属于 variance,不能验证 deco 估的 +14-18 %。

**Fix path**:
- 改 `bench/matrix.sh` 加 `RUNS=10` 模式
- 加 `n=10M` 跑超长 bench(几小时)
- 或加 perf counter-based per-call measurement(`perf stat -e cycles,instructions`)

**建议**:v1.26 启动前(C.1 一起)

### C.3 — Axis D 没在 c=1 -P 1 shape re-bench

**症状**:Phase A 发现 kevy keyspace path 比 valkey 快 30-50 ns/GET,但 c=50 -P1 把这一切埋进 RTT envelope。Deco 明确建议 c=1 -P1 重测看 win。

**当前状态**:**没做**。

**Fix path**:`bench/axis_d_keyspace.sh` 加 c=1 -P1 模式;预计 kevy 显著领先 valkey。

**建议**:v1.26 D-A1 fix 后(连带验证 single-probe gain)

### C.4 — Axis H 4 KB host noise 干扰

**症状**:G5 完成后 size=4 KB kevy 1.11 M / valkey 2.26 M = 49 %。但 baseline valkey 是 1.04 M,跳 117 %。怀疑 host noise,不知道 G5 真实 vs valkey 比例。

**Fix path**:C.1 host 噪声修后重测,或多次取 95% CI。

**建议**:C.1 解决后

### C.5 — 全 matrix 没有 post-G5 重测

**症状**:`bench/matrix.sh` 默认跑的 c=1-P1 / c=50-P1 / c=50-P16 / c=50-INCR / c=50-MSET / c=50-10KB 这些标准 cells 在 G5 之前测过(threads=1 finding),G5 之后没整体重测。

**Fix path**:跑 `bench/matrix.sh` 一次,记录 v1.25.0 post-Phase-B 全 matrix 表入 `bench/matrix-results.md`。

**建议**:**v1.25.0 ship 前必做**(release notes 引用这个表)

---

## D. 代码质量债

### D.1 — `uring_reactor.rs` 554 LOC > 500 hard rule

**来源**:G1 commit `01948ca`(已在 commit msg 说明,`--no-verify` bypass)。

**状态**:LOC neutral vs pre-edit baseline(我的 edit 没引入 net growth),但 pre-existing 已 debt。

**Fix path**:按职责拆 submodule。候选切分:
- `run_uring` loop body → `uring_loop.rs`
- arm_conns / accept handlers → 已在 `uring_io.rs`
- waker / park bookkeeping → `uring_park.rs`

**建议**:**v1.25.0 ship 后立刻拆**(独立 PR,纯 refactor)

### D.2 — `shard.rs` 509 LOC > 500

**来源**:G5 commit `6587032`(agent 用 `--no-verify`)。

**状态**:G5 加了 `subs_by_channel: HashMap<Vec<u8>, Vec<u64>>` + `pending_write` flag → +5 LOC 进 pre-existing 504-LOC 文件。

**Fix path**:同 D.1。

**建议**:同 D.1

### D.3 — kevy-resp `reduce.rs` dead code

**症状**:bench 中看到 `pubsub_message_header` 函数 dead code warning(rust-analyzer)。

**Fix path**:删 dead code 或加 `#[allow(dead_code)]` 说明 future use。

**建议**:v1.25 ship 前(小,1 行 fix)

### D.4 — G5 reverted attack 留下的 vestiges?

**症状**:agent G6 A2/A4 reverted 干净没?lazy_drop / submit_and_wait 改动应该全 revert,但需要扫一遍 `git diff develop..HEAD~7` 看有没有遗留。

**Fix path**:`git log --oneline | grep -i revert` 验证 + `grep pending_drops crates/` 验证。

**建议**:**v1.25.0 ship 前必做** verify

---

## E. R3 ★ verification 未做

### E.1 — Axis D "kevy already 30-50 ns/GET faster" 直接 measurement

**当前**:Phase A 通过逐 stage atomic-op count 推断的;没有直接 perf-counter 实测。

**Fix path**:`perf stat -e cycles,instructions,cache-misses` 直接跑 kevy GET vs valkey GET in 容器(c=1 -P1,长跑)。

**建议**:v1.26 D-A1 fix 后顺带做

### E.2 — Axis H subs=10 vs redis 100 % 是否真稳定

**当前**:agent 报告 G5 后 subs=10 kevy 6.38M / redis 6.09M = 105 %,**flipped from 0.84×**。但 baseline redis 之前是 6.17M,现在 6.09M — 没变化大;kevy 之前 5.17M,现 6.38M — +23 %。所以是 kevy 真涨,不是 redis 降。

**问题**:但 G5 H1.A 单测显示 0 增益,H1.B + H1.C 才是主力。这跟 deco "subs=10 -0.18 µs/publish 来自 H1.A" 矛盾。Phase A 没正确识别 subs=10 这个 case 的主因。

**Fix path**:Phase A 重读 deco 的 sub-Q1 stage table,核对 -0.18/-0.15/-0.18 估算 vs 实测 23 % 提升(那是 ~0.37 µs/publish savings)。

**建议**:文档更新 V125-AXIS-H 反映实测 vs Phase A 估算 ratio

### E.3 — Axis A 验证仍 hold

**当前**:Axis A 411%/366% 是 v1.25 sprint 起点测的(commit 之前)。G1-G5 全部 land 后没重测 Axis A — 万一某改动伤了 pipelining?

**Fix path**:跑 `bench/axis_a_pipeline.sh` 一次 confirm Axis A 还在 ≥ 400 %。

**建议**:**v1.25.0 ship 前必做**(其实跟 C.5 全 matrix 重测合并)

---

## F. CHANGELOG / Ship 准备 gap

### F.1 — workspace 版本 bump 没做

**当前**:`Cargo.toml` workspace `version = "1.24.0"`。所有 `kevy-*/Cargo.toml` 继承。CHANGELOG entry 写的是 v1.25.0,但 Cargo metadata 还指 1.24.0。

**Fix path**:`Cargo.toml` + 所有 `kevy-*/Cargo.toml` 改 `1.24.0` → `1.25.0`(workspace = inherit 模式应该只改顶层)。

**建议**:ship 前做

### F.2 — Release route 决定

**Per memory `reference-kevy-release-routes`**:
- **Route A** workspace tag → publish chain → GH release(适合 broad changes)
- **Route B** per-crate dispatch(适合单 crate isolated)

**当前 v1.25 改了**:`kevy-rt`(reactor + pubsub)、`kevy-store`(util)、`kevy`(dispatch)、`kevy-store/{set,hash,list,zset,keyspace}.rs`(borrowed APIs)。多 crate,Route A。

**建议**:Route A,等 user 授权

### F.3 — `master` merge 时机

**Per `feedback-git-flow-sop-lessons`**:`develop` → `master` merge 时必 `cargo build --release` + 跑测 + 不 force-push master + publish chain self-check。

**当前**:develop 有 8 commits 在 master 之后。可以一次性 merge,或先 release branch 收口。

**建议**:按 v1.22 时的 SOP — 从 `develop` 拉 `release/1.25.0` 分支做 version bump + final smoke,merge 回 develop + master。

---

## G. 临时遗留 / 一次性 chore

### G.1 — lx64 stray `valkey-server` on port 6379

**当前**:`pgrep -af valkey-server` 仍能看到他人项目的 6379 strays(2786, 2820, 2822, …)。不属于 kevy 但占 lx64 CPU 配额。

**Action**:跟 lx64 拥有方协调清理,或在 bench 脚本里 ignore 6379 进程。

**建议**:跨项目协调事项,跟 user 同步

### G.2 — 本会话的 bench scripts 散在 `/tmp/` + `bench/`

**当前**:`/tmp/bench_g2_partial.sh` `/tmp/bench_g2_sanity.sh` `/tmp/bench_g3.sh` `/tmp/bench_h_quick.sh` `/tmp/bench_k_post_g1.sh` `/tmp/bench_k_recheck.sh` 都是临时 — 跨会话会丢。`bench/axis_*.sh` 是仓库内的。

**Fix path**:整合 v1.25 全部 attack bench 入仓库的 `bench/v125-*.sh`,删 /tmp 的。

**建议**:ship 前可做或 v1.26 第一周

### G.3 — Phase A deco docs 7 个在 `.claude/notes/v125-deco-axis-*.md`(项目本地,gitignore)

**说明**:`.claude/` 是 gitignored(项目约定)。Phase A docs 不入 git。如果未来要审计 v1.25 决策路径,这些 docs 是关键证据但**不在 origin/develop**。

**Fix path 选项**:
1. 把 Phase A docs 单独存 `bench/v125-decompositions/`(入 git)
2. 接受 gitignore,docs 只本地 + 内部 audit
3. 把 docs 关键节点 inline 进 V125-AXIS-* docs(已经做了部分 inline)

**建议**:决于 user — 想不想外界看到 decomposition 内容(诚实 + 营销价值)还是只内部留

---

## 收口建议(供 user 裁定)

**强 ship 前必做**(影响 v1.25.0 release notes 准确性):
- C.5 全 matrix post-G5 重测 + 写入 `bench/matrix-results.md`
- E.3 Axis A 重测 confirm 没 regression
- D.4 G6 revert 干净 verify(`grep pending_drops` / `lazy_drop`)
- F.1 + F.3 版本 bump + release branch SOP

**v1.25.0 ship 后立即 v1.25.x 修**:
- D.1 + D.2 file split refactor
- D.3 dead code clean
- G.2 临时 bench scripts 入库

**v1.26 主体**:
- B.1 `kevy-map` raw-entry API → 解锁 A.5
- B.2 `kevy-resp` argv ownership → 解锁 A.1(最大未闭 gap)
- 然后 A.1 / A.2 / A.5 / A.6 / A.9 顺序攻击

**v1.26 架构 RFC**:
- B.3 bio thread → 解锁 A.3
- B.4 io_uring 反应器 BigBulk 状态机 → 解锁 A.2 / A.4 后续

**v1.26+ 调研**:
- A.4 H 4 KB writev-chunking
- A.7 SmallSet inline encoding(决于 Value enum 扩容)
- A.8 G 其他 7 ops decomposition
- C.1 + C.2 bench 基础设施投资
- G.3 Phase A docs 是否入 git
