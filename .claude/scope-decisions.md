# kevy scope decisions

Project-level "what's IN vs OUT of scope, and why" log. Append-only —
older entries stay as historical context, newer entries refine the
boundary. CLAUDE.md links here for non-obvious calls.

---

## OUT of v1.0 — and beyond

### Bare-metal MCU (`no_std`) port

**Decided:** 2026-05-27, by user

**What this excludes:** Cortex-M3/M4/M7 (STM32, nRF52, RP2040), ESP32-S2,
RISC-V MCUs (ESP32-C3, GD32V), and similar "no operating system,
running directly on hardware" embedded targets with typically
16-512 KB SRAM.

**Why:** porting kevy would be a full rewrite, not an adaptation:
- `std` not available → must move to `#![no_std]` + `alloc` (or
  `heapless` for bounded compile-time collections)
- No default heap allocator → need explicit `linked_list_allocator`
  config or all-static layout
- No threads / no OS scheduler → kevy-rt's thread-per-core reactor
  is meaningless; embedded mode would also need single-task rewrite
- `format!` machinery is a code-size grenade on MCUs (tens of KB)
- Stack typically 1-4 KB; would need full audit of recursion + locals
- Cortex-M0 has no atomics at all; M3+ has only a subset
- 32-bit pointer width (same blocker as wasm32, but only one of many)

This is "port to a different product" scale, not "add a feature".
SQLite's MCU support took years of engineering on a dedicated branch.

**What we DO support that the user might call "IoT":** Linux SBCs —
Raspberry Pi 4/5, Jetson Nano, Rock Pi, OrangePi, OpenWrt high-end,
Buildroot, Yocto. These ship full `std` + glibc/musl and run kevy
**with zero changes** on `aarch64-unknown-linux-gnu` or
`aarch64-unknown-linux-musl`.

**Re-evaluate:** only if a paying customer asks specifically for bare-
metal MCU. The right answer at that point would likely be a separate
`kevy-tiny` fork, not in-tree refactoring.

### AUTH / TLS — OUT of scope (do NOT put in backlog/roadmap)

**Decided:** 2026-05-27, by user (v1.0 scope discussion)
**Re-confirmed + refined:** 2026-06-03, by user (after v1.1.0 ship)
**2026-06-07 — firm OUT, by user (multiple times):** stop listing AUTH/TLS
as a roadmap candidate or "conditional-IN" backlog item. The user has said
this repeatedly. It is **not** an open work item. Treat it like Cluster /
Replication: permanently out unless the user *themselves* reopens it. Do
not surface "should kevy add AUTH/TLS?" as a next-step suggestion. The
rationale below is retained as history, NOT as a deferred to-do.

Deferred to v0.3+ / v2 timeline. Target deployment scenarios
(docker-compose internal network, kubernetes pod network, embedded
in-process, browser/WASM, cache layer fronted by trusted upstream)
all have the trust boundary at the *network* level, not the database
level. Matches valkey/redis default behavior (no `requirepass`).

Mitigations in v1.0:
- `bind` defaults to `127.0.0.1` (loopback only)
- Startup WARN if non-loopback bind is set (`kevy WARN: bind=… is
  not loopback and kevy has no AUTH/TLS yet.`)

Re-evaluate when: public internet exposure becomes a real target, or
multi-tenant kubernetes deployment with untrusted neighbor pods is
in scope.

---

**2026-06-03 refinement — when re-evaluated, the path is "optional features", NOT a workspace-wide pivot.** The user briefly explored a v3 boundary opening AUTH+TLS as core scope; rejected it the same session as "反我最开始做 kevy 的想法" (counter to the kevy original intent — lightweight, pure Rust, 0 deps, single-machine cache / embedded). The agreed shape for *when* AUTH+TLS eventually land:

- **AUTH** — `requirepass` + `AUTH` command. **No feature flag needed.** Pure string comparison + connection-state machine, fits the 0-deps + pure-Rust charter as a normal feature. Add it like any other command family when the trigger fires.
- **TLS** — **`tls` cargo feature, disabled by default.** Default kevy stays 0 deps (the L2 promise holds for the headline build). Enabling `--features tls` opts the user into `rustls` (or whatever the right pure-Rust TLS choice is at that point) for accept/connect paths. ACL / multi-user → a separate `acl` feature, even later (v3.1+).

**Trigger condition (unchanged):** a real public-internet or multi-tenant scenario shows up. **Not** general "kevy should be production-ready" — production-readiness for the four scoped scenarios (dev / docker-compose / embedded / cache) is already met by loopback bind + persistence, not by AUTH/TLS.

**Why this matters in the doc:** future sessions surfacing "should kevy add TLS?" should land here and find that (a) the boundary stays where it is, (b) when crossed, the shape is opt-in features not a workspace pivot, (c) the trigger is concrete deployment shape, not a vague "make it secure".

### Replication (single-machine primary→replica)

**Decided:** 2026-05-27, by user · **Reopened 2026-06-18 as part of v3 cluster scope expansion** (see Cluster mode below).

~~Cut from v0.2 / v1.0~~ — superseded. Replication is now IN as `kevy-replicate`,
the carrier for the v3 cluster (primary→N replicas + quorum failover + embed-as-member).
See `.claude/rfcs/2026-06-18-v3-cluster.md`.

Original cut reasoning preserved for context:

- docker-compose: single instance is the norm; HA = upgrade path to
  k8s StatefulSet + persistent volume
- embedded: in-process library, no "replicate" concept
- cache: upstream DB is the source of truth, cache rebuild acceptable

What changed in 2026-06-18: mailrs dogfood close-out surfaced two pressures
(read scaling beyond a single 16-core box; embed↔server shared state) that
the cut reasoning didn't anticipate. Replication moved from "not needed for
the four target scenarios" to "the mechanism that unifies embed-as-member,
read scaling, and server-backed embed-writer-fallback".

### Cluster mode

**Decided:** prior session, by user · **Amended 2026-06-18, by user** (see v3 cluster RFC)

#### IN (v3 cluster series)

- **Single-primary + multi-replica with quorum failover.** N+1 nodes,
  primary takes writes, replicas serve reads, automatic primary election
  on DOWN via `kevy-elect` quorum (no Raft).
- **`READCONSISTENT` per-command hint** for read-your-own-writes
  semantics (eventual by default).
- **embed-as-replica + embed-as-writer (scoped multi-writer).** An embed
  process may join the cluster as a read-replica or, in Phase 3, as the
  sole writer for a key-prefix-declared scope, with a server-backed
  fallback if it crashes.
- **read/write-split client** (`kevy-cluster-rw`) that fans reads to
  replicas and writes to the scope's owning writer.
- **Replication** as `kevy-replicate` (offset-based AOF stream +
  initial snapshot ship).

#### Still OUT (permanently)

- **Sharded multi-master**: two writers may not own overlapping
  scopes. The model is non-overlapping declared scopes, not active-active.
- **Cross-DC active-active / CRDTs**: single-DC, single-partition
  assumption throughout v3. Cross-DC = different cluster, app-level
  federation.
- **Online resharding**: topology is config-declared; change requires
  rolling restart of affected nodes.
- **Gossip-style discovery**: topology is config-declared, not learned
  via gossip (Redis Cluster's bus). Quorum failover uses the declared
  peer list.
- **Raft / strong-consistent log replication**: F3 quorum is the
  cheapest correct primary election for single-DC.
- **AUTH / TLS** across cluster traffic: same trust-bounded-network
  deployment model (see AUTH/TLS section above).

**Clarified 2026-06-10, by user:** the original OUT meant *multi-machine
distribution* (failover, MIGRATE/ASK, gossip, cross-machine keyspace —
all still permanently OUT under the strictest reading). A **single-machine
CLUSTER-protocol compatibility subset** (read-only CLUSTER SLOTS/SHARDS/
NODES + MOVED redirects + per-shard ports, CRC16 slot mapping) shipped
v1.13 as the vehicle for key-aware shard routing. v3 builds **above**
this single-machine layer, not in place of it. See
`.claude/rfcs/2026-06-18-v3-cluster.md`.

## v4 可用性 arc REFUSED(2026-07-05 plan 批准)

- 多节点 slot-sharding / gossip / MIGRATE / ASK —— 水平扩展是另一个 arc,不与 failover 语义搅和;拓扑至 v4 为止 = 1 primary + N replica。
- 独立哨兵进程 —— 选举内嵌(kevy-elect);代价(HA 最小 3 数据节点)入 docs。
- replica 自动补位/自动重建 —— 编排器职责;kevy 只出原语(REPLICAOF + snapshot resync 已全)。
- 动态成员变更(节点热添加/joint consensus)—— 成员表 operator 静态供给;动态的只是角色。
- 全局同步复制模式 —— WAIT 提供 per-request opt-in quorum 确认,足够。
- 级联复制(sub-replica)/ replica 迁移 —— 目标规模(<16 replica)无收益。
- 基础客户端(kevy-client/-async)全自动 failover —— 最多补 reconnect;HA 客户端 = kevy-cluster-rw 单一路径。
- fencing token 外宣为分布式锁服务 —— (gen,offset)/epoch 留门不承诺,等实证信号。

## AI 友好轴 REFUSED(2026-07-05 3.x 总线 plan 批准)

- HTTP/REST API 面 —— RESP + MCP 双接入面已够;不养第三个协议面。
- server 端 embedding 生成 —— 模型推理不进 kevy;向量由应用侧生产。
- LangChain / LlamaIndex 官方集成包 —— 社区面,等 demand 信号再议。

## v4 T7 追加 — no_std stone 能力 vs 整机 MCU 端口(2026-07-12)

T7b no_std spike(判决书 `.claude/notes/k101-nostd-verdict-2026-07-11.md`)按
蓝图要求对上面 2026-05-27 的 "Bare-metal MCU (`no_std`) port — OUT" 旧决做证据
重审。**结论:旧决维持 OUT,但需精确区分两件事**:

- **stone core 已 no_std-capable = DONE(不是重开 MCU 决)**:五个石头 crate
  (`kevy-store`/`kevy-hash`/`kevy-bytes`/`kevy-map`/`kevy-madvise`)已
  `#![cfg_attr(not(feature="std"), no_std)]` + `alloc`,thumbv7em-none-eabihf
  check 在 CI;无 64-bit 原子的 ISA 走 `external-clock` feature 的 AtomicU32×2
  seqlock 时钟。这是**把石头层做成可被 no_std 宿主复用的库**,不是"kevy 整机跑
  在 MCU 上"。A1b F1/F2/F3 又补实了裸 core 构建契约(见审计台账)。

- **整机 MCU 产品端口 = 仍 OUT(旧决全部理由不变)**:kevy-rt 的 thread-per-core
  reactor、format! 代码体量、单任务重写、栈审计等在上面 2026-05-27 条目列的阻塞
  全部成立。石头 no_std 化不触碰这些——它只让下游 no_std 项目能 `kevy-store =
  { default-features = false, features = ["alloc","external-clock"] }`,不等于
  kevy server 能在 MCU 上 serve。

因此蓝图 REFUSED 页脚里 "no_std" 一词的口径已在本条澄清:被 REFUSED 的是
**整机 MCU 端口**,不是**石头层 no_std 能力**(后者是 v4 T7 交付项)。

## v4 T9 追加(2026-07-12)

- **L1 shared-read keyspace(seqlock 读道)= REFUSED**:原型 gate 三判据
  两过一败(撕裂 0 ✓ / 重试 p99=0 ✓ / 单 op 节省 0.02-0.06µs,距 0.3µs
  门 5-10× ✗)。根因不是 seqlock 不行,是 kevy 转发链在高批密度下已近
  最优(batch16 链模拟 ≈ 裸单 op hop——RequestBatch 摊销把 ring 往返
  摊没了),per-op 可省常数天花板 ~0.1µs,收益/blast(数周级:表级版本
  +epoch bucket swap+写后读 fence)不成立。shared-nothing 铁律(写读
  都 shard-owned)维持原表述。重开条件:workload 真变(真 spread +
  高读写比 + owner 饱和),且判据改整机 A/B。原型证据 =
  crates/kevy-bench/examples/seqlock_probe/(入树)。

## v4 T3 追加(2026-07-11)

- **SQPOLL 默认接入 = REFUSED(实测判决)**:K-307 spike,lx64
  A/B(c50/c100 × GET/SET,median-of-5,同 pin 布局)全四格大负:
  GET c50 -86% / SET c50 -83% / GET c100 -64% / SET c100 -64%
  (base ~2.1-2.2M rps → sqpoll 0.3-0.8M)。机理:每 shard 一个
  iou-sqp 内核轮询线程继承 cpumask,与 shard busy-poll 抢同一核集,
  有效 CPU 减半。判决线是 +3%,差距不可辩。`KEVY_SQPOLL=1` env
  开关保留(默认 OFF,measurement-only),重开条件 = 布局假设变化
  (空闲核部署 / new_sqpoll CPU pin / 内核演进)。证据 =
  bench/PERF-FINDING-2026-07-11-sqpoll-refused.md。
