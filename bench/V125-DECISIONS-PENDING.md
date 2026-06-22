# v1.25 — 待 user 决策的两个架构事项

> Attack queue 里有两项 ($106 A.7 SmallSet 内联, $105 A.3 bio thread) 在实施前
> 需要 user 拍板架构走向。本文档列选项 + 影响,**不替 user 做判断**(per
> `feedback-utility-judgment-not-mine-on-kevy`)。

---

## A.7 — Value enum 扩容?

### 现状

`crates/kevy-store/src/value.rs:162` 硬 cap:
```rust
const _: () = {
    assert!(std::mem::size_of::<Value>() <= 32);
};
```

理由(file:line 注释):
> 收集变体 (Hash/List/Set/ZSet/Stream) 走 Arc 而非 inline,所以 enum 只跟最大
> inline 变体 (`Str(SmallBytes) = 24 B`) + tag 一样大 = 32 B。每 `Entry` ≈ 48 B
> 而不是 ~80 B,**bucket array 密度 ~40% 更高(大 keyspace cache miss 更少,
> RSS 更小)**。多一次 pointer chase 只在 collection ops 上,不在热 GET 路径。

### A.7 attack 目的

Axis G SADD `redis-benchmark` 默认 `-r 0` shape:每次 SADD 同一字面量,set 永远
1 个 member。valkey 跑 `OBJ_ENCODING_LISTPACK` 1-entry mode = 1 cache line。
kevy 的 `KevySet` = 16-slot Swiss table 最小,**结构上无法 match 1-cell listpack**。

要 match valkey 小 set 路径,需要 `SmallSet<...>` inline variant on `Value` enum。

### 选项

| ID | 设计 | Value enum size | 每 Entry 大小 | bucket array 密度变化 | inline set 容量 | 备注 |
|---|---|---|---|---|---|---|
| **O1** | 不扩容,SmallSet 用 23 B 内联([u8; 23] 紧凑序列化) | 32 B(不变) | 48 B(不变) | 0%(不变) | 1-3 个 tiny member (≤ 23B total) | 部分 match listpack 1-entry case;但 N≥4 仍 heap |
| **O2** | 扩到 48 B | 48 B | 64 B | **-25% 密度(+33% 大)** | 4-6 cells | match valkey listpack 大部分 case |
| **O3** | 扩到 64 B | 64 B | 80 B | **-37% 密度(+66% 大)** | 8-10 cells | match valkey 全部 listpack;但 keyspace cache miss 变多 |
| **O4** | SmallSet 走 Box<SmallSetData>,enum 还 32 B | 32 B(不变) | 48 B(不变) | 0% | 64-128 B by Box(任意) | 1 次 heap alloc + 1 次 pointer chase per access;失去 "inline 赢 Arc" 论据 |
| **O5** | 等 valkey-style encoding 转换:小 set 用 inline (O1),大 set 自动转 KevySet | 32 B(不变) | 48 B(不变) | 0% | O1 + 自动转换 | 复杂度增加,但同时拿到 inline 小集 + 大集 Swiss table 性能 |

### 影响估算

- O1 直接做:5K LOC 工程量,gain 仅在 valkey-shape `-r 0` 1-entry bench 显著
- O2/O3 影响所有 store cache layout — 需要重测全部 axis(可能 GET 路径变慢)
- O4 引入 collection 路径性能退化(原本 inline 设计就是为了避免这个)
- O5 最贴近 valkey 模型,但需要 encoding switch 逻辑 + Phase A 重新决定

### 决策点

用户:**O1 / O2 / O3 / O4 / O5 / 其他?**

如果 O1:本 attack #106 可在不破坏 cache layout 前提下 ship。
如果 O2/O3:需要全 axis 重 bench 验证不破坏既得 wins(c1-P1 147% 等)。
如果 O4:跟 valkey listpack 路径同模型,但与 kevy 设计哲学冲突。
如果 O5:正式立 v1.26 Phase A,本 attack #106 跳过。

---

## A.3 — Bio thread 架构

### 现状

`crates/kevy-rt/src/runtime.rs::Runtime::run` — thread-per-core,每 shard 一个
busy-poll reactor 线程。**完全没有后台 bio thread**。

### A.3 attack 目的

G6 A2 lazy-drop big values reverted(G6 实测 -3.4% throughput + p999 +144 µs)。
真因(deco 已记):valkey `lazyfree.c` 之所以赢是因为 **separate bio thread**
在后台 free,**deferral 本身无意义,要有 thread 把工作真带走**。要 unblock
A.2 lazy-drop 必须有 bio thread。

### 架构 RFC 选项

| ID | 设计 | 隔离粒度 | 通信 | NUMA/亲和性 | 复杂度 |
|---|---|---|---|---|---|
| **B1** | per-shard bio thread (N shard = N bio thread) | 高(每 shard 独立) | shard-local channel(无锁) | 自动跟 shard 同 socket | 高(N×) |
| **B2** | 单全局 bio thread,所有 shard 共享 | 低 | MPSC kevy-ring | 单 socket fixed | 低 |
| **B3** | 单全局 bio thread per NUMA node | 中 | 每 NUMA SPSC | 自动 NUMA-aware | 中 |
| **B4** | thread pool (cores=N_cpu - N_shard) | 灵活 | global work-stealing | manual config | 高(需任务窃取) |

### 关键架构决策点

1. **Value Send 边界**:今 Value 类型不要求 `Send`。bio thread 用 Value drop 需要 Value: Send。审视 Hash/List/Set/ZSet 内部数据是否 Send。
2. **busy-poll vs sleep**:bio thread 要不要 busy-poll(占 CPU 跟 reactor 抢 cycle)还是 condition_var sleep(高延迟 wake)?
3. **优先级**:bio drop 是低优先;但如果 bio queue 积压,内存涨。要不要 backpressure?backpressure 到 reactor 就退化成 inline drop。
4. **drop 之外还能干啥**?bio thread 一旦有,可以做 background BGREWRITEAOF / BGSAVE(目前 server SAVE/BGREWRITEAOF 在 shard 线程同步阻塞,memory `Perf campaign (embed ceiling)` 末注:"server 的 SAVE/BGREWRITEAOF 仍是 shard 线程内同步");可以做 active TTL reap;可以做 slow cmd offload。

### 影响估算

- B1: N shard = N+N=2N 线程。CPU 占比翻倍但每 shard 隔离,无 cross-shard 同步。
- B2: 1 全局 thread,所有 shard 共享。简单但 contention 风险。
- B3: 跟 NUMA topology 走。kevy 当前部署都单 socket(lx64 / mailrs prod),没有现实 NUMA 案例 — over-engineering 风险。
- B4: 灵活但 work-stealing 实现 ~1k LOC unsafe / 引入复杂同步原语。

### 决策点

用户:**B1 / B2 / B3 / B4 / 其他?**

也可:
- **B0**: 现在不做 bio thread,接受 A.2 lazy-drop 反向(已 reverted),把 A.2/A.3 完全从 v1.25 队列删
- **跟 BGREWRITEAOF/BGSAVE migration 一起做**(memory 记的"比真 COW 便宜的 80% 解 = 把三段式移植到 server")。bio thread 既解 lazy-drop 又解 BGSAVE 阻塞 — 一鱼两吃 更好 ROI。

---

## 怎么用本文档

User 看完上面两题,挑选项(可用"O2"/"B2"这样回复)或提其他设计。我落地。

如果倾向跳过其中之一,直接说"跳过 A.7" / "跳过 A.3" 也可 — 不算 R8 defer(user
判断 = 项目锁定 / 合法 filter)。

---

## 2026-06-22 — 决策(user 委派"按项目原则决策吧")

### A.7 → **O5(valkey-style encoding switch)**

- **O2/O3 violate `value.rs:162` Value enum 32B hard cap** — 直接撕 -25-37%
  bucket density,核心 cache layout 设计在文件注释里明示("Entry-48B
  win")。要扩需 RFC 级讨论,跟当下 perf 攻击 scope 不齐。
- **O4 Box<SmallSetData>** 引入 heap alloc + pointer chase per small-set
  access,违反 Value enum 注释的"inline 赢 Arc"设计哲学。
- **O1 inline-only** 不足以 match valkey listpack 在 N≥4 时;部分覆盖
  case 但 G axis bench shape 仍有问题。
- **O5 valkey-orthodox encoding switch**(SmallSetInline 23B 紧凑序列化
  for N≤一阈值,自动 grow 到 KevyMap)= 完整 match valkey listpack/HT
  路径,符合 `feedback-greenfield-advanced-compat`(behavior compat,
  modern core)+ `feedback-orthodox-no-shortcuts`(不发明新轮子)。

实施:Value enum 加 `SmallSetInline([u8; 23], u8 used, u8 count)` 变体
(或类似 23-25B 包),sadd/srem 操作触发 encoding upgrade KevyMap;先做
SET (SADD) pilot 然后扩 HSET/HMSET/ZADD/LPUSH/RPUSH。

### A.3 → **B2(single global bio thread)**

- **B3 per-NUMA** = 当前单 socket lx64/prod 的 over-engineering
  (`feedback-orthodox-no-shortcuts`)。
- **B4 thread pool work-stealing** 需要 ~1k LOC unsafe + 复杂 sync,违反
  "纯 Rust + safe by default" 倾向 + 增加 review/audit 难度。
- **B1 per-shard** 在 `--threads N` 时 2N 线程,跟 thread-per-core 模型
  叠加资源压力;同时 BGSAVE 通常一个全局就够,不需要 per-shard。
- **B2 single global bio thread** = 最简 valkey-orthodox(valkey 本身
  也是 1 bio thread for fsync + lazyfree)+ MPSC 队列用 std 即可
  (kevy-ring SPSC 或 std::mpsc;前者已在 use)+ 一线程的 contention
  后续遇到再 partition。**符合 orthodox-no-shortcuts** + 不增 dep。

实施:Runtime::run 在 spawn shards 之前 spawn 1 个 bio thread + 持有
SPMC 入口(per-shard producer → 1 bio consumer);Value: Send 边界
audit;bio thread 跑 drop + fsync queue;重做 A.2 lazy-drop(把大
Value 排队进 bio 而非 inline drop)。预期 Axis I tail max 真正闭合
(B.4 R3 ★ 指出 130-160ms outliers 是 sync Drop,不是 memcpy)。

兼带顺势改:server-side SAVE/BGREWRITEAOF migration(memory 记的
"比真 COW 便宜的 80% 解 = 把三段式移植到 server")可以走同一 bio
thread,unblock 现有 shard-sync 阻塞 disk 写的 known 问题。

### 实施顺序(per #105 → #106 → #107 序列 / blocked 关系)

1. A.3 B2 bio thread 基础设施 + A.2 lazy-drop 重做(unblock Axis I tail max + 可能解 Axis H 4K 剩 26%)
2. A.7 O5 SmallSetInline encoding(pilot 用 SADD)
3. A.8 G 其他 6 ops(HSET/HMSET/ZADD/LPUSH/RPUSH/LRANGE — 用 SmallSetInline 同模式扩)
4. SET-with-options / MSET / SETEX / APPEND / GETSET 的 BigBulk 扩展(B.4 agent 报告 scope only 是 *3 SET)
5. 不 ship(user 红线)。完整 perf 后再统一评估 ship。
