# kevy roadmap

唯一的开放工作清单。**线性化的执行顺序** — 上面的先做，下面的后做。

最后更新：2026-07-03(深夜;**user 新指令:中途不 release,v3 彻底做完后一次性 release**。v2.1.0 已发为最后的中途 release;此后每 train 收口 = 五轴 gate + merge develop,版本号/tag/publish 全部攒到 v3.0 终点一次性做)
当前状态：**已发布基线 = v2.0.20**(workspace 全 2.0.20;kevy-embedded 1.15.0 独立 track)。develop == master,同步 origin,干净。
v1.x → v2.0 的完整历史账目见 git log + `CHANGELOG.md` + `.claude/plans/2026-06-30-v2-roadmap.md`(已完成归档)。

## 当前 arc — v3 serving engine

- **设计文档**:`.claude/plans/2026-07-03-v3-serving-engine.md`(六能力平面 P0-P5 / D1-D6 承重决策 / §4b RDS on-ramp)
- **边界 constitution**:`.claude/plans/2026-07-03-rds-refugee-services.md`(三定律 / 迁移旅程 Stage 0-5 服务表 / REFUSED 表)
- **视图设计**:`.claude/plans/2026-07-03-views-design-exploration.md`(LOCKED 2026-07-03)
- **最终计划(含 audit A1-A7)**:`~/.claude-profile-2/plans/staged-splashing-truffle.md`

定位:应用放弃 RDS 后把主数据模型直接建在 kevy 上的 serving engine。Field feedback(mailrs 等)一律作为 RFC 输入,**不进入逐条回信循环**。

## 运营铁律(违者返工)

**feature 分支先行(2026-06-10 立)**:任何条目开工第一动作 = 建 feature 分支,然后才写第一行代码。develop 只允许 trivial CI/docs 一行改和 feature 的 merge。merge 用 `--no-ff`。autorun 授权只解除"逐步等批准",不解除 git-flow。

**RFC gate(v3 arc)**:标【RFC】的 train,RFC 批准前不写实现代码(Phase A/B 分离)。

**五轴收口(2026-07-03 立,ceiling-first)**:每 train finish 前五轴全绿,gate 红 = 不许 finish;基线只升不降(ratchet,同 perfgate 哲学):

1. **perf** — `bench/perfgate.sh` 全绿(既有线 + 本 train 新增线);基线仅在有意改进落地后 `--update-baseline` 抬高,绝不为绿灯而改。
2. **mem** — 本 train 每个新子系统:RFC 声明内存公式 → lx64 实测 bytes/entry 对公式(±20%)→ 超预算 = create/rebuild 清晰报错(资源自适应)。`bench/memgate.sh` 承载。
3. **disk** — AOF / catalog / feed retention / index checkpoint 磁盘增长边界声明 + 30M-key 包络实测(rewrite 停顿、replay 吞吐同口径)。`bench/diskgate.sh` 承载。
4. **doc** — 无 doc 不 finish:docs/*.md + README 三语(user-visible 时)+ CHANGELOG entry + RFC 归档标状态。
5. **cov** — CI `cargo llvm-cov` job(外部 cargo 工具,零 Cargo.toml 依赖,不破 0-dep):新 crate(kevy-index/text/vector)行覆盖 ≥ 90%;workspace 总覆盖率 v2.1 记基线后只升不降。

**用户授权(standing)**:上限性能优先,不考虑 ROI / win/risks,正统思路指导,自动决策技术问题,遇分叉按上限最高选项 + 禁野路子。资源自适应哲学:空间大释放性能、空间小不挂、大范围合理卡、超额清晰报错+警告+闸门。

---

## 线性 checklist(v2.1 → v3.0,从上往下,不跳序)

> 每 train:开工第一动 = feature 分支;标【RFC】的 RFC 批准前零实现代码;finish 前五轴收口全绿。embedded surface 变化随 train 同步 bump kevy-embedded minor。

### v2.1 — P0/P1 foundation ✅ SHIPPED 2026-07-03(v2.1.0 / embedded 1.16.0——最后的中途 release)
- [x] 分支 `feature/v2-1-op-foundation`
- [x] op 枚举 sweep:全 op × 6 surface(server dispatch / embedded Store / Pipeline / AtomicCtx / replay / rewrite_fmt)现状矩阵 → gap doc
- [x] const OP_TABLE(名字 / 读写类 / key 位置 / surface eligibility / wake-blocked 类 / Lua 写类;纯数据,非 codegen,热路径零改动)
- [x] 各 surface manifest + CI parity test(缺失 (op,surface) 对列名报红)
- [x] cmd.rs 三分类清单 / cmd_block.rs 唤醒清单 / cmd_lua.rs 写清单 → 对表穷尽性测试
- [x] AtomicCtx 补全(atomic + atomic_all_shards):del/hdel/zrem/sadd/srem/lpush/rpush + hgetall/hmget/hexists/zcard/exists + sweep 余量
- [x] 条件写:ZaddFlags GT/LT/NX/XX/CH 全 surface;SET 命令 flags 面统一核查(setnx/hsetnx 已确认存在,audit A2)
- [x] 原子性 charter doc(单 shard / all-shards 确定序锁语义(audit A4 有源码依据)、blessed serving 配置、ceiling)
- [x] 耐久契约:appendfsync × atomic-commit 矩阵 doc + per-block fsync barrier opt-in
- [x] covgate 建制:CI 加 `cargo llvm-cov` job + 记录 workspace 覆盖率基线(ratchet 起点;audit A6:现无 coverage job)
- [x] memgate/diskgate 脚手架:`bench/memgate.sh` / `bench/diskgate.sh` 骨架 + 首批线
- [x] 五轴收口 → release **v2.1.0**(embedded 1.16.0)

### （新插入,进行中）task #9 收尾 + task #10 perf campaign

- [x] v2.0.21 hotfix 对 SHIPPED;v2.1.0 SHIPPED(最后的中途 release)
- [x] `feature/v2-1-1-uring-fallback` CI 全绿验收 → merged develop(CI 9/9,v2.0.17 以来首次)
- [x] **task #10 perf campaign 闭环(2026-07-04)**:两台阶具名(286c4a2 v1.17 INFO 计数器 -4% 模态;4fa4631 nap 移除单 commit -20%)→ 三假说带遥测证伪 → **stay-hot-while-inflight** 修复 merge develop:legacy 双角恢复+超越(get +9% over 旧基线)、pinned 全保、-c1 反升 80k@15µs、perfgate 6/6 原基线真 PASS → 基线诚实重录(pinned +18% 与 legacy 恢复双向入账)。考古:`bench/PERF-FINDING-2026-07-03-…` + `PERF-DECOMP-2026-07-04-…` rounds 1-4。尾巴(低优先):286c4a2 微机制、epoll stay-hot 对称

### v2.2 — P3 zset 代数(Redis parity)
- [x] 分支 `feature/v2-2-zset-algebra`;store 纯代数(WEIGHTS/AGGREGATE 全 Redis 6.2 语义)
- [x] set 代数 `*STORE` 形式补齐(SINTERSTORE/SUNIONSTORE/SDIFFSTORE)
- [x] server 命令(gather+二跳 store 编排,rename-orchestrator 模式;CROSSSLOT numkeys-aware)+ embedded facades + OP_TABLE 7 行 + effect-AOF 豁免;zrange_by_score LIMIT embedded 变体(server 本有)
- [ ] 五轴收口(mem✓ disk✓ doc✓ clippy✓;perfgate 含新 zalg 角 + CI cov 在飞)→ merge develop

### v2.3 — P4 offset 脊柱(CDC + 恢复点)【RFC】
- [ ] RFC:`(generation, offset)` 语义 / at-least-once / prefix filter / FEED.* server 面 / retention 默认+上限 / kevy-replicate backlog 复用
- [ ] user 拍板 → 分支;embedded changes_since/changes_tail + server FEED.*
- [ ] 世代号跨 rewrite + 超 backlog 干净 resync 错误
- [ ] 恢复点契约("snapshot S + (gen,O) = exact restore point")+ restore drill doc + PITR 路径
- [ ] info_prefix(embedded + server)
- [ ] lx64 实测:30M keys rewrite 停顿 + replay 吞吐 → docs/persistence.md + metric sink(= diskgate 首批正式线)
- [ ] 五轴收口(perf:feed lag p99 < 100ms 常驻线)→ merge develop

### v2.4 — P4 flow round-out + 读视图
- [ ] 分支;embedded blocking pop(timeout,park-wait 设计短文先行)
- [ ] zpopmin_below(store + server + embedded)
- [ ] hash-field TTL(HEXPIRE/HPEXPIRE/HTTL/HPERSIST):per-field deadline + reaper + AOF/snapshot + 全 TTL replay 矩阵(正确性敏感)
- [ ] 公开 SnapshotView:一致性 point-in-time prefix 迭代(写不停,读冻结)
- [ ] 五轴收口 → merge develop

### v2.5 — P2 索引引擎核心 ⭐【RFC + design round】
- [x] RFC(最大):catalog 持久化 / prefix hook 空 catalog 零税方案 / DSL 文法 + 标量 coercion(parse 失败=排除并入 verify 报告)/ Range+Unique 语义 / derived-by-construction(AOF 只记 primary write)/ rebuild-verify-fsck / 组合 + cursor 契约(SCAN 式保证,snapshot 指向 v2.4)/ IDX.* 命名空间 / 内部 keyspace 对 TYPE/SCAN/DBSIZE/KEYS 不可见 / per-index 内存预算公式
- [x] mailrs design round(输入非工单)→ user 拍板
- [x] 分支;新 crate kevy-index(stone):catalog + hook + DSL
- [x] Range kind + Unique kind
- [x] index_query + AND/OR 组合 + cursor 分页
- [x] Hydration ⭐(IDX.QUERY ... FIELDS + embedded 等价:一跳解引用 + 声明字段投影)
- [x] index_count(count-without-fetch)
- [x] rebuild_index / verify_index → Report / index stats;index_query 进 SLOWLOG
- [x] 五轴收口(perf 双闸:空 catalog 0% 回归 + index_query p99 < 2ms @ 1M rows + hydration 线;mem:index 公式实测;cov:kevy-index ≥ 90%)→ merge develop

### v2.6 — P3 views(虚拟/物化视图)【RFC】
- [x] RFC:ViewSpec(组合树 over 命名索引 / order_by / mode / hydrate)/ 三条结构规则(只存成员+序、组件显式命名+视图层零谓词、partition 为查询参数)/ VIEW.* 命名空间 / 可交换重排条款原文 / top-K underflow 从基索引局部重算 + K+Δ 缓冲 / Via 栅栏(≤2 跳、纯模板、目标无谓词、目标缺失=nil)
- [x] user 拍板 → 分支;Virtual 模式(VIEW.QUERY / VIEW.EXPLAIN / view-tag SLOWLOG;统一查询入口:IDX.QUERY ⊂ ad-hoc ⊂ named view)
- [x] Materialized 模式(And=写时成员检查 / Or=refcount / Diff=检查;增量维护)+ top-K 有界
- [x] Via 多跳 hydration(SORT GET 先例)
- [x] verify_view / rebuild_view / view stats
- [x] 五轴收口(perf:VIEW.QUERY virtual p99 < 3ms @ 1M×2 组件;物化读 = 索引读同线;写路径税:3 基底 4 物化视图 < 15% vs 裸索引;mem:视图公式)→ merge develop

### v2.7 — P2 text kind(CJK FTS)【RFC】
- [x] RFC:Tokenizer trait / unicode 分段 + CJK bigram 无词典默认 / BM25 / prefix+field 查询 / 内存公式 / 重建边界
- [x] user 拍板 → 分支;新 crate kevy-text(stone,feature-gated);挂 kevy-index;hydration 统一
- [x] 五轴收口(perf:p95 < 20ms @ 1M docs;mem:公式实测;cov ≥ 90%)→ merge develop

### v2.8 — P2 vector kind(ANN)【RFC】
- [x] RFC:HNSW 参数 / partition filter / 内存公式(1M×1024d ≈ 4GB 包络)/ bounded rebuild 契约
- [x] user 拍板 → 分支;新 crate kevy-vector(stone,feature-gated);挂 kevy-index;hydration 统一
- [x] 五轴收口(perf:kNN p95 < 30ms @ 1M;disk:checkpoint/rebuild 时间对契约;cov ≥ 90%)→ merge develop

### v2.9 — P4 拓扑【RFC】
- [x] RFC:fork —— (a) embedded RESP listener(read-only v1)vs (b) embedded-as-primary 复制(骑 feed);工程 merit 定
- [x] read-your-writes token:写返 (gen, offset) opt-in + 读路径 wait_for
- [x] user 拍板 → 分支;impl 按 RFC;两进程同数据集 e2e
- [x] 五轴收口 → merge develop

### v2.10 — RDS on-ramp(迁移工具链)
- [x] 分支;kevy-cli import(pipelined + 续传 + 原子批)+ --verify(行数 + per-prefix checksum)
- [x] kevy-cli export(逻辑、prefix-scoped、line 格式、read-view 一致)
- [x] deferred index build(load 挂起维护 → 末尾一次 rebuild)
- [x] prefix bulk ops(copy/delete by prefix,限速 + 续传)
- [x] prefix digest(DEBUG DIGEST genre)+ kevy-cli diff
- [x] kevy-cli inspect(prefix browse / index inspect / IDX.EXPLAIN 诊断器)
- [x] 五轴收口 → merge develop

### v2.11 — P5 验证 arc(serving 尺度实证)
- [x] 分支(test-only 也走);serving-shape perfgate 常驻套件汇总(row-list p99<1ms / 写扇出 p99<200µs / 各 train 闸线)
- [x] 尺度 soak(lx64):30M keys / 1M index rows / 1M vectors + 物化视图混载;复核 v2.3 声明边界
- [x] index-fsck + view-fsck chaos(写中 kill → verify → rebuild → 零差异)
- [x] 五轴全量复核(mem/disk 实测 vs 全部已声明公式,一张对账表)
- [x] 五轴收口 → merge develop

### v3.0 — serving engine declared（一次性 release 点）
- [x] RDS→kevy modeling cookbook(refugee doc §4 全清单:布局 / 序列 / 乐观锁 / CHECK 替代 / 幂等键 / 软删 / 复合排序 / NULL / JSONB 指南 / 级联 recipe / outbox 不需要 / 审计 via CDC / 反向镜像 / 分析导出)
- [x] docs arc:"designing your app on kevy" + 六平面总览 + 三定律公开版
- [x] README 三语 headline + serving charter 汇总(原子契约 / 恢复点契约 / 全 gate 线)
- [x] CHANGELOG v3.0 总账;workspace → 3.0.0;GH Release manifesto;全 crate publish
- [x] 五轴终审 → ship **v3.0.0**

---

## 谨慎评估后不合并(v3 范围外)

**完整 REFUSED 表见 `.claude/plans/2026-07-03-rds-refugee-services.md` §5**(17 项,每项标注违反哪条定律 + 需求由什么覆盖)—— 今后同类 ask 直接引用,不逐案重议。其中两行经 views 设计探索 refine(见 views doc §5):查询语言 body / planner 展开 / 视图层谓词仍拒;命名组合 spec + 声明模式 = IN(genre 是索引,不是存储的查询)。views 侧另有 4 项永久拒绝 + 3 项 v3.x 推迟(views doc §4)。

三定律速记:**Law 1** Redis 契约不可破(parity 用 Redis 名 + 精确语义;新 genre 走 `IDX.*`/`FEED.*`/`VIEW.*`/`FT.*`/`VEC.*`;索引/feed/视图内部对 Redis surface 不可见)。**Law 2** superset 五点判据(通用 over opaque bytes / 写时声明式 / 访问路径显式命名 / 派生态可重建可校验 / genre 先例)。**Law 3** RDS 事件视界(meaning 和 planning 永不进引擎)。

## v∞(永久 OUT-of-scope,见 `.claude/scope-decisions.md`)

- Sharded multi-master、cross-DC active-active / CRDTs、Raft、在线 resharding、gossip
- **AUTH / TLS**(user 多次确认不做,不要再放回 backlog)
- `no_std` MCU 端口
- (v3 栅栏)SQL / joins / cost-based planner;写路径回调;AuthZ/租户语义;跨存储事务

## 参考(按需 load)

- `.claude/plans/2026-07-03-v3-serving-engine.md` — arc 设计(六平面 / D1-D6 / §4b)
- `.claude/plans/2026-07-03-rds-refugee-services.md` — 三定律 + 服务目录 + REFUSED 表
- `.claude/plans/2026-07-03-views-design-exploration.md` — 视图设计(LOCKED)
- `.claude/plans/2026-07-03-serving-core-roadmap.md` — mailrs 12 项 vs 源码 audit 表(SUPERSEDED but §2 有效)
- `.claude/CLAUDE.md` — 项目规则;`.claude/scope-decisions.md` — OUT-of-scope 历史
- `GIT-FLOW.md` — feature/release/hotfix SOP;`bench/REPORT.md` — public benchmark narrative
- `.claude/plans/2026-06-30-v2-roadmap.md` — 上一 arc(v1.36→v2.0)归档

---

# v3.x perf arc(用户拍板 2026-07-04:不开 v4,全部收在 3.x;**最终版本号目标 = v3.8.0**;主设计 = .claude/plans/2026-07-04-v4-perf-arc.md,编号按本表为准)

北极星:对标并远超 valkey(裸面)+ Redis Stack(serving 面)。
纪律:不许自家 baseline 当标尺;先测后攻(双 gate);对标物版本入账。

### v3.3 — 基线 arena(measure-only)
- [ ] 分支 `feature/v3-3-arena`;lx64 装 redis-stack 最新 GA(版本入账)
- [ ] 矩阵定义:裸面 7 角 + serving 面 4 类(FTS/ANN/agg/numeric-range)× 语料规格(沿 gate 语料认识论:Zipf/流形)
- [ ] median-of-5 + sample stdev 全跑(两端同协议同盒)
- [ ] 产出 bench/PERF-LEDGER.md 初版真 gap 表 → 五轴(doc 轴)→ merge
- [ ] **gap 表评审:据数据修订 v3.4-v3.7 train 内容**(写回本表)

### v3.4 — tails 清偿 + 真上限复测(gap 表评审 2026-07-05:裸面全胜 1.67-2.5×,无 gap)
- [x] client-bound 上限复测 — **疑似不成立**(2026-07-22):`bench/clientbound.sh` 扫客户端宽度,线程 4×/核 1.5× 吞吐平(±2.5%),生成器 CPU 峰值 698%/800% 始终有余量 → 7.2M 是**服务端**上限,arena 比值分母是真的。入 PERF-LEDGER
- [x] 286c4a2 -4% micro-mechanism 根因收口 — **CLOSED,该开销已不存在**(2026-07-05,A/B no-op 两个热路径钩子,见 `bench/PERF-FINDING-2026-07-05-tails-closure.md` Case 1)
- [x] epoll stay-hot 对称性收口 — **FIXED**(同上 Case 2:`shard.rs` idle ladder 在 `xshard_inflight > 0` 时重置 `idle_spins`,与 uring 侧 65b7515 对齐)
- [x] IDX.QUERY conn-tail 根因收口 — **CLOSED,根因 = accept 落位**(SO_REUSEPORT 选中的属主分片与它的 extension fan-out 角色冲突;v1.30 `--accept-shards` 完全消除。见 `bench/PERF-FINDING-2026-07-04-idxquery-conn-tail.md`)

### v3.5 — FTS 巩固(gap 表评审:p95 NOISE、qps +21% 已胜;自 ratchet 改进)
- [ ] 单常见词 postings scan → impact-ordering(textgate 加线)

### v3.6 — ANN 攻坚 ⭐(gap 表评审:**唯一真 gap,stack 3.8× 领先** —— 本 arc 主战场)
- [ ] Phase A0:recall-latency pareto 对齐(两侧 EF 扫描同 recall 档比延迟)重量 gap
- [ ] Phase A:decomposition vs RediSearch HNSW 实现(读源码,18+ stage)
- [ ] Phase B:attack(worktree)→ vectorgate + arena 复测

### v3.7 —(缩编:agg/query 全胜 110×/2.3×,并入 v3.8)

### v3.8 — 终账(**最终版本 v3.8.0**)
- [ ] PERF-LEDGER 定稿;新线全部进 perfgate ratchet;CHANGELOG 总账 → **v3.8.0 = roadmap 终版**;其后 D/A/B 方向另立新程

---

# 实证 arc(D→A 拍板 2026-07-05;主设计 = .claude/plans/2026-07-05-dogfood-arc.md)

分工:mailrs 侧用户亲自做;kevy 侧支撑与响应。零 release 至 v3.9.0。

### v3.9-t0 — release 完整性 + 升级文档(用户指令 2026-07-05)
- [ ] crates.io publish 完整性核查(全 member 对账 + docs.rs)
- [ ] docs/UPGRADING.md:2.x → 3.x 升级指南(breaking/新面/步骤)非常清晰
### v3.9-t1 — onramp drill(迁移工具链全链自演练,产 UX 缺口清单)
### v3.9-t2+ — mailrs 实证反馈 train(按需;真实语料反哺 gate)
### v3.9-tF — 收口:cookbook 实证版 + v3.9.0(用户拍时机)

---

---

# 3.x 总线(plan 批准 2026-07-05;主设计 = .claude/plans/2026-07-05-3x-mainline.md;**最终版本号目标 v3.17.0**)

线性从上往下;零 release 至 v3.9.0,其后零 release 至 v3.17.0。mailrs 反馈 train 随到随插(优先级高于当前列)。单一事实源纪律:verb 元数据表 = COMMAND DOCS = llms.txt = MCP schema,CI 对账。

### v3.9 — 实证 arc(mailrs 侧用户做)
- [ ] t1 onramp drill:mailrs 形状(多前缀/大 value/TTL 混布/1M)全链 export→import --resume --strict→digest/diff→后建索引 backfill→copy/delete-prefix --rate;缺口清单,小缺口即修
- [ ] t2+ mailrs 反馈 train(输入驱动;真实语料反哺 gate)
- [ ] tF:cookbook 实证版 + arena 复测 + CHANGELOG → **v3.9.0**
- [ ] 悬案:lx64 盒策略(用户拍)→ perfgate floor 复验;master CI 确认

### v3.10 — AI 操作面·契约机读化【RFC】
- [ ] verb 元数据表(名称/arity/flags/参数签名/摘要;含全部 IDX./VIEW./FEED.)+ COMMAND LIST/COUNT/DOCS 实装 + CI parity(dispatch ⊆ 表)
- [ ] 扩展错误自解释化(ST_* → 具名 -ERR)+ error-replies.md 补扩展段
- [ ] IDX.EXPLAIN(与 VIEW.EXPLAIN 同构结构化)
- [ ] RESP3:HELLO 3 真值协商 + 扩展查询 map 化(RESP2 不变)
- [ ] aigate 一期(DOCS 全覆盖对账/错误具名/EXPLAIN 结构)→ 五轴 merge

### v3.11 — AI 开发面·文档机读化(轻列)
- [ ] llms.txt + llms-full.txt(与元数据表同源生成)
- [ ] docs/verb-reference.md 单页全 verb(三语)+ README 挂链
- [ ] cookbook 可执行化(recipe 自包含块 + CI smoke job)
- [ ] kevy-embedded rustdoc 83%→100% + missing_docs ratchet
- [ ] aigate 二期(llms.txt ↔ COMMAND DOCS 一致性)→ merge

### v3.12 — 官方 MCP server【RFC】
- [ ] 新 crate kevy-mcp(stdio JSON-RPC,纯 std 自研;schema 由元数据表生成;只读默认/写 opt-in)
- [ ] aigate 三期(MCP e2e:list_tools/call_tool 全矩阵/错误保真)→ 五轴 merge

### v3.13 — AI 存储场景·hybrid 检索 + 场景包【RFC】
- [ ] IDX.QUERY HYBRID(KNN×MATCH RRF 融合,server 端归并;对标 RediSearch/Qdrant fusion)
- [ ] agent-memory 场景包(session-TTL/episodic 双索引/RAG chunk+hybrid recipe,全部可执行)
- [ ] aigate 四期(融合质量 clamp ≥ 单路 recall + recipe smoke)+ perf ratchet(hybrid p95)→ 五轴 merge

### v3.14 — 可用性 A0:复制地基 + 分区注入【RFC】
- [x] replica READONLY gate(来源分流/默认 on/内生写策略)
- [x] REPLCONF ACK wire(状态模型 = slot.rs acked_offset)+ 复制流心跳(replica 自感知 lag)
- [x] replica 三态状态机(-LOADING 策略)+ INFO/ROLE 全做实 + min-replicas-to-write(辅助)
- [x] kevy-chaos socket 代理注入(切断/延迟/半开/方向性非对称)+ SIGSTOP 独立轴
- [x] availgate 一期 → 五轴 merge(2026-07-06;-LOADING 未做:无发射点,error-replies 已除行)

### v3.15 — 可用性 A2:failover 闭环(计划内→崩溃式)【RFC,主战场】
- [x] 阶段一 计划内 FAILOVER(quiesce(-QUIESCED 复用)→追平→epoch bump 交棒;零丢失 clamp)
- [x] 阶段二 崩溃式:选举硬化(epoch/votedFor 先落盘再应答+崩溃重投用例;按 ack offset 择优)→ ANNOUNCE→数据面接线 → 重启角色解析 clamp(v3.16 收紧为 election-only write authority)→ fencing 落地形 = promotion→generation bump + Future→snapshot ship 分叉丢弃(握手 EPOCH 字段方案被此形替代)→ 降级旧主回 -READONLY(零新动词)
- [x] availgate 二期(3 进程+分区 e2e:MTTR=选举超时 K 倍/分叉丢弃/WAIT-acked 零丢失/重启角色)→ 五轴 merge

### v3.16 — 可用性 A1+A3:一致性等级 + quorum lease【RFC】
- [x] read-your-writes token((gen,offset);cmd_block parked;超时 -MISDIRECTED)
- [x] WAIT n timeout + bounded-staleness(-STALE;cluster-rw 读池剔除)
- [x] primary quorum lease(主项;自我 fence;停写窗口计入丢失上界)
- [x] availgate 三期(12 clamps:WAIT 真值/RYW 20 轮/-STALE/quorum fence+heal;注:时钟跳变格未做——elect 全用 Instant 单调钟,挂钟跳变不影响;WriterPool 核对由 clamp 内 SET/GET 断言代)→ 五轴 merge

### v3.17 — 总终账(**最终版本 v3.17.0**)
- [x] availgate+aigate+repligate 进 CI(contract-gates job+lx64 全量);SLO 表(逻辑不变量+分位数 ratchet)
- [x] docs/availability.md;arena 复测见下 全矩阵复测 + PERF-LEDGER 更新
- [x] CHANGELOG 总账;workspace → 3.17.0;ship(tag/GH/crates/smoke)= 3.x 总线终版

REFUSED:可用性 8 条照旧 + AI 轴 3 条(无 HTTP/REST 面;无 server 端 embedding 生成;无 LangChain/LlamaIndex 官方包)——入 scope-decisions。

---

# v4 入口总线(用户拍板 2026-07-14:包做实 + 渠道发行 + 移动三框架 + 竞品对比;主设计 = .claude/plans/2026-07-13-v4-entrypoints-arc.md + 本表)

现状基线(2026-07-14 盘点):六语言 embedded 门功能面全落地且 ffigate
每 push 双 OS smoke 绿;但「正式包形态」(prebuilt 分发物 + 装机验证)、
渠道真发行、移动真机 e2e、竞品对比全部未做。client 面只有 Rust 自家
(kevy-client / kevy-client-async);其他语言靠 RESP 兼容吃现有 redis
client 生态 —— 该策略 t2 里 gate 化。

### t1 — 包做实:embedded 面正式包形态(无拍板项,先行)
- [x] npm @goliapkg/kevy-node prebuilt 打包:kevy-napi .node × (darwin-arm64 / linux-x64 / linux-arm64) + bun cdylib 同装;packaging/npm 加脚本;tarball 装机 smoke(node + bun 双跑) —— 打包脚本(`gen-node-platform-pkg.sh`)+ 离线装机 smoke(`smoke-node.sh`:打 tarball → file: 装 → node+bun 双跑)本机 darwin-arm64 跑通;**接进 CI 新 job `npm-install-smoke`**(matrix: macOS-arm64 + ubuntu-x64,两 RID 持续验发布布局)。linux-arm64 无托管 runner,留渠道发布时自托管盒补(已注明)。真 `npm publish` 属 t6
- [ ] NuGet Kevy.Embedded 打包:runtimes/<rid>/native 三平台 cdylib;dotnet pack + 本地 feed 装机 smoke —— **打包形态尚未成形(2026-07-23 CI 实测暴露)**:`pack-and-smoke.sh` 对 `Kevy.Embedded`(IsPackable=false 的内部门)跑 pack,产不出包;真要发布的是 `Kevy.Client`(id=`kevy`),但它经 ProjectReference 引内部门,NuGet 默认不打进包 → 装 `kevy` 拿不到 `KevyDb`。这是发布形态设计缺口(需 pack target 打包内部门 DLL + runtimes cdylib),归 t6,详见 `packaging/nuget/PACK-GAP.md`。(脚本已加 `ls feed/*.nupkg` fail-fast 断言,正是它抓出真相;误加的 CI job 已回退)
- [x] Go module 发布形态核对(libs/<target>/libkevy_ffi.a 内嵌布局 + go.mod 门面);pkg.go.dev 门面 README —— 核对结论写入 `bindings/go/PUBLISH-FORM.md`。**当前 cgo 序言两处 `../..` 相对路径(header + `target/debug/libkevy_ffi.a`)在模块剥离后必断链**,发布形态 = 内嵌 `include/` + `libs/<goos>_<goarch>/libkevy_ffi.a` 三平台 release,序言改按 target 解析。**一个必须发布前拍板的决定**:go module v2+ 要求路径带 `/vN` 后缀,kevy-go 是采 `/v4` 镜像引擎版本、还是自起 v1 —— 留渠道发布 owner(t6)
- [x] 各包 README(npm / NuGet / SwiftPM / Maven / go)= 装法 + typed 面 + cmd 逃生门 + 版本对齐 4.0.0 —— 五份都齐 typed 面 + cmd 逃生门;**修了一个诚实问题**:除 go 外四份都让用户跑今天不存在的装包命令(dotnet add / npm install / SwiftPM from:"4.0.0",而最新 tag 只到 v3.18.0),现五份都标注 4.0.0 且都给出 pre-release 说明 + 仓库内可跑的替代路径(本地 feed / path 依赖 / run-tests.sh)
- [x] ffigate 升级:六门断言对齐一张契约表(cmd 面 / error-as-data / pubsub / durability / 标量快路) —— `bench/ffigate-contract.sh`(6 门 × 5 行 = 30 格,表在脚本里即契约本身),接进 CI 的 ffigate job 最前面(纯源码检查,秒级失败)。**查出两个真缺口并已补**:① C++ 的 `kevy.hpp` 根本没有暴露标量 get/set,C++ 调用方只能掉到 C API —— 已加 RAII 包装(`std::optional<std::string> get` / `set`,miss 是 nullopt 不是错误)并在 smoke.cpp 断言,本机 c++17 实编跑绿;② Bun 门的 `bun.js` 有 `getScalar`/`setScalar` 但测试从没碰过 —— 已补断言(含"标量写入也要能重开后存活"),本机 bun 跑绿。反例验证:破坏任一格 → 只有该格 MISS 且退出 1

### t2 — client 面:兼容矩阵 gate 化(RFC 已内含推荐,自研与否留拍板)
- [x] clientgate:主流 redis client 连 kevy server 的兼容矩阵进 CI —— node-redis / ioredis / go-redis / StackExchange.Redis / hiredis / redis-py × (基本 KV + 扩展面 raw 通道 IDX./VIEW./FEED.);async 由各生态 client 自带覆盖 —— `bench/clientgate.sh` + `bench/clientgate/`(六个 client 各一份),CI job `clientgate (six redis clients, one kevy server)` 每次推送跑,run 29955415164 绿
- [x] docs:「bring your redis client」页(六语言连接示例)+ Rust 自家双 client 挂链 —— `docs/clients.md`
- [ ] 拍板项:是否自研 per 语言 typed client(暴露 IDX./FEED. typed 面)—— 推荐不做,RESP 兼容即生态;要做则另立 train

### t3 — 移动做实:RN(expo + bare)+ Flutter(工具链本机已备:Xcode/模拟器/NDK;Flutter SDK 待装)
- [x] expo-kevy example app + mobilegate 一期:iOS 模拟器 + Android 模拟器双端 e2e smoke —— **双端 ALL PASS**(2788f735/9ac1303e);真机抓到 open() file:// URI bug 已下沉库层;mobilegate.sh 用 simctl/adb logcat 独立捕获 verdict
- [x] bare RN 验证:expo-modules-core in bare RN 路线跑通 + 文档;不可行则 TurboModule 壳兜底 —— `bindings/expo/barern-example`(RN 0.86 / SDK 57,Podfile+gradle 手工接线 expo-modules autolinking,mirror `expo prebuild`)。**本机实跑双端全绿**(2026-07-23):barern/android PASS + barern/ios PASS。过程修了两个 gate bug:barern android_cmd 未锁设备(会装进插着的真机)+ 端口提示挂死(`--no-packager`)
- [x] flutter_kevy:dart:ffi 直连 kevy-ffi cdylib,federated plugin(android jniLibs + ios xcframework);smoke 进 mobilegate —— `bindings/flutter`(dart:ffi 绑定 + ffigen + 双平台 + test/)完整存在;mobilegate flutter 分支真跑 `flutter run`(iOS debug 模拟器 / Android release 模拟器)。**本机实跑双端全绿**(2026-07-23):flutter/ios PASS + flutter/android PASS(后者 ANDROID_SERIAL 锁定模拟器,避开插着的真机)
- [x] mobilegate 二期:三框架(expo / bare RN / flutter)all-green 一张表 —— `bench/mobilegate-all.sh`:driver 跑三框架×双端六格,各捕获 verdict,产出 PASS/FAIL 网格,任一非 PASS 整体退 1。**六格本机当场全绿(2026-07-23)**:expo/ios+android、barern/ios+android、flutter/ios+android 全 PASS。(developer/CI-on-macOS gate,非 per-push;需 booted 模拟器+emulator,ANDROID_SERIAL 锁定避真机)

### t4 — 竞品对比:mmkvgate + embedded bench(性能北极星 = 全轴超越 MMKV)
- [ ] mmkvgate:iOS XCTest measure + androidx microbenchmark;轴 = 同步标量 get/set × value 尺寸 × 冷/热 + 批量 + 启动加载;对手 = MMKV 原生 + react-native-mmkv(RN 层再对一轮);数字如实入账,输的轴列明
- [ ] 输轴 → decomposition + attack(perf-vs-foss 方法论,2 轮 polish 不动针即 decomp;目标全轴 ≥ MMKV)
- [~] embedded bench RFC:竞品名单+轴 **RFC 已写**(`.claude/rfcs/2026-07-23-v4-embedded-bench.md`,四路竞品调研入账,marketing vs 第三方已标)。**一处更正 ROADMAP 候选**:C# 侧 LiteDB **降级为文档存储参考**(它无原生 KV 路径,只有 collection Insert/FindOne=品类错配),真正公平的 C# KV 对手 = **LMDB via Lightning.NET**(原生 mdb_put/mdb_get 同步)。选定:Go=bbolt(读型)+badger(LSM/append 近 kevy AOF);Node=better-sqlite3(同步,真 bar)+classic-level(异步,标记为跨模型参考);C=LMDB(读延迟王,最硬 bar);C#=LMDB via Lightning.NET。**公平性框架**:durability 三层(T-mem/T-async/T-fsync,只同层比)/ 同步异步不同表 / 冷单操作 vs 摊销双轴 / 值尺寸扫 16B-64KB + 冷启动。**四语言 harness 全建成 + 本机相对位次全测**(2026-07-24,bench/embeddedgate/{node,go,c,csharp},bench/EMBEDDED-LEDGER.md 四轨 + 跨轨综述):对五引擎(SQLite/bbolt/badger/LMDB×2)。**普适规律**:kevy 胜单操作读(kevy_get_shared 零拷贝 lane 平 ~12ns、**反超读延迟王 LMDB 2-9×**)+ 单操作小写(无每 op txn,12-62×);**输 bulk/batch 写**(每引擎都赢,bbolt 单事务 147×/LMDB 37×/SQLite 3.8× @64KB —— kevy 无 batch-write 路径)+ 大单写双拷贝 + 拷贝型绑定的读(Go/C# byte[] 拷出,C 轨道证零拷贝 lane 才是引擎真优势)。**攻击 #1 batch-write 已 BUILT+MEASURED**(2026-07-24):`kevy_set_many` FFI 原语(batch.rs,一次 crossing N sets,durability 不变)+ 接入 kevy-go `SetMany`(字节界 ~1MiB C arena,oversized 回退)+ Kevy.Embedded C# `SetMany`。**实测推翻"batch 处处闭合缺口"**:bulk 缺口是**引擎级**(C 无 crossing + C# 廉价 P/Invoke 都停 ~200ns vs LMDB ~80ns,set_many 均不动),根因 = kevy **每 op 格式化 AOF RESP frame**(它的 durability 模型:每写即入可恢复日志)vs LMDB 延迟到 commit 的 dirty-page 写 —— frame 格式化本质 per-op、batch 无法减。set_many **只帮昂贵-crossing 绑定**:Go cgo(50-100ns/op)从 **147×→2.70×**;C# 无救(廉价 crossing,本就引擎受限,arena 反微增)。**闭合引擎缺口 = 改 durability 模型(per-op 日志→事务/commit)= persistence-core RFC**(高 blast,设计轮先行,非 autorun)。测试:FFI 往返/misuse + kevy-go/C# SetMany 往返(含 2MiB oversized 回退)全绿。攻击 #2 零拷贝绑定读 lane 仍开(niche + lifetime-safety,defer)。LiteDB 品类错配(C# 轨 LMDB 验证)。**lx64 定值 pass 待**。**拍板项**:engine durability-model batch-commit RFC 是否开(真 bulk 天花板)/ Node napi setMany(可选确认点,非 ceiling)/ lx64 时机 / classic-level 保留
- [ ] server 面 vs valkey 沿用 arena/perfgate 常驻,不重复建

### t5.5 — durability trust arc(mailrs P0 引子;用户拍板 2026-07-17:全量进 v4、no defer、报告内外全覆盖、排 ship 前)
- RFC:`.claude/rfcs/2026-07-17-v4-durability-trust.md`(域分解 14 轴 + AOF envelope v2 承重决策 + 关闭项账目)
- [x] T1【gate 先行】crashgate(SIGKILL 注入矩阵)+ persistence.md 状态机契约(丢失上界表 / 多 shard 偏斜 / feed×截断审计成文)—— 先红后修
- [x] T2 corrupt tail:quarantine 拷贝 + truncate(闭 docs:194 文档谎言)
- [x] T3 可观测:OpenReport + Replay{dropped_bytes,corrupt} + INFO 字段 + WARN 首因文案 + kevy_open_report 过各门
- [x] T4 生命周期:Store::shutdown()(幂等 clone 安全)+ rewrite 绝对/时间阈值 + RewriteStats 公开 + fsync/rewrite 策略过 C ABI 与各门
- [x] T5 AOF envelope v2(len+CRC32C+sync marker;v1 只读、rewrite 升格式)+ 流式 replay(峰值 O(最大记录),消 open 双读;memgate/diskgate 上线)
- [x] T6 resync:v2 确定性重扫(strict 默认 / best-effort 可选 + resynced_ranges 上报)+ v1 启发式 + 损伤注入 fuzz
- [x] T8 复制世代栅栏(握手/ACK 带 gen 6-args wire;fence 落 pump 先于 caught-up 检查,顺手修 mid-stream bump stall/aliasing 第二张脸;runner data_gen + 心跳 gen 断链;embed-writer per-boot nanos gen;side-audit = resync 后本地 AOF 同步 rewrite re-base;测试 = embed_writer_e2e 重启 aliasing + kevy replication unclean 重启 + rw_split gen 化 + repligate clamp 5 全 PASS;附带修 T3 回归 OpenReport 属主入 DropGuard)
- [x] T7 docs/runbook/UPGRADING/CHANGELOG(修订 disk-format 表述)+ mailrs 回信账目
- 关闭项(有据非 defer):3.x backport 不做(指令:全在 v4 解决);大值写放大存储模型改造不做(mmap-AOF 已实测否决)
- [x] T5b wasm host-mediated log 升 v2(arc 内新增:双格式嗅探 / fresh 流自带 magic / dump 即升级点;bit-flip 在浏览器侧也被 CRC 拒收)
- [x] **arc 五轴收口(2026-07-18)**:crashgate 全绿(硬门,CI PENDING_STRICT=1)/ diskgate PASS(v2 开销 112 vs 106 B/op = +5.7%,20% band 内;rewrite 24ms 持平)/ replaymemgate(峰值 O(最大记录),37MB 日志实测 RSS 4MB)/ covgate ratchet 不降 / docs 三语收口 + mailrs 回信账目 / **perfgate PASS —— 12 指标全绿,SET 轴 +0.2%/-1.9%/-0.8% 全在噪声带,v2 每记录 CRC32C 零可测吞吐代价**(交错测量 vs ref 349cafc1,box drift 单列抵消)
- 附带产出(非 RFC 计划内,均为实测驱动):**两个 io_uring 数据面 bug 根治** —— ① SQ 满时 recv 重臂丢失致连接 wedge(`76c79c38`);② multishot `res=0` 带 `F_SOCK_NONEMPTY` 被误判 EOF(`667005f9`,lx64 真内核 A/B **4/25 挂 → 0/25**)。另修 perfgate 自身两处(`3f99373b` set-u 下 ref_binary 中止;`7a87a1aa` 空 BIN pkill 曾三次打挂 lx64)。finding:`bench/PERF-FINDING-2026-07-18-uring-recv-rearm-wedge.md`

- 附带产出二批(2026-07-19,同样实测驱动):**一个真可用性 bug 根治** —— replica 链路的 I/O 错误(EPIPE)被 `?` 抛出 reactor,**杀掉一个 replica 会杀掉 primary 的 shard**,连带关闭其上全部无关客户端连接;唯一痕迹是 `shard N exited with error: Broken pipe`。修后三处统一(pump 写路径 / epoll 事件路径 / uring tick 路径),**availgate 19 clamp、四 phase 全绿**(此前多个 release 红在 phase-4,被一条误导消息掩盖)。另两个连接级 bug:阻塞超时漏退休 seq(`8d8f20e9`,uringgate 780 轮 0 FAIL,与 reactor 无关)、chunked writev 短写重发已发送前缀(`17c7062f`)。clientgate 两处 pub/sub 竞态修复后 CI 转绿。
- gate 质量债一并清:recv 遇 EOF 必须报错而非空转(14 脚本 26 处;原状态在共享盒烧一个核 4.5 小时)/ availgate seeder 退出码检查 + 失败自带证据 / killgate 进 CI(插值 `pkill -f` 必须有紧邻空值守卫)/ perfgate 拒 root + per-run scratch + **baseline 重录(原缺 ref_commit,从干净 clone 根本跑不了)**。**教训:gate 的失败消息质量 = 调查效率的上限。**

### t5.6 — 消费者信任收口(goliajp embedded-as-primary-store 报告引子;排在 ship 之前)

引子 = `docs/REPORT-FROM-GOLIAJP-2026-07-20-EMBEDDED-AS-PRIMARY-STORE.md`
(对方正拿 embedded 当薪资系统主存储)。已修:D1 拒绝事务回滚 /
D2 事务标记(全有全无,与事务大小无关)/ R2 事务内集合读。
**顺序原则:先把「验证」变便宜且诚实,再交付别人在等的,再收自己归档的,最后回发布列车。**

- [x] **A1 pushgate** — 一条命令在本地跑 CI 上全部「不需要起服务」的门(clippy 用 CI 的确切命令 / locgate / commentgate / killgate / docs+site parity / doc configs / CJK / vendorgate),并**显式打印它不覆盖的 CI 步骤**(uringgate / availgate / covgate / repligate / mobilegate…)。
      根因:本地跑的命令集 ≠ CI 的命令集,导致「一轮红一个门」;且我曾把 `--all-features` 当成 CI 命令,而 CI 并没有这个 flag。子集门必须自曝子集边界,否则就是又一个隐形悬崖。
- [x] **A2 fmt —— 决定不采纳**(2026-07-21)。查证:仓库**没有 rustfmt.toml**,`cargo fmt --check` 报 **519 个文件 / 2642 处**(395 src + 96 tests)。这不是漂移,是本项目从不用 rustfmt、风格手工维护。采纳意味着重写 519 个文件 + 冲掉 git blame + 撞所有在飞工作;不采纳是零风险现状。CLAUDE.md 的代码质量规则只管文件/函数长度(locgate 已守),不含 fmt。pushgate 不跑 fmt,保持不跑。**要改成采纳需用户明确批准**(那是有代价的方向)。
- [x] **A3 三轮同码全绿** — v4「验证完备自洽」的达标线,**达标(2026-07-23)**。同一 commit `f5e0293c` 三次独立 CI 执行全绿(run 29981355519 attempt 1/2/3 = success),证明这份代码稳定无脆测。前置 = 本 session 修好的三个真 flake:escrow 回归(LRANGE 去竞态)/ spop_storm(patience 旋钮 + 副本饿死诊断)/ C1(write-result,三 reactor 确定性)——正是"验证完备自洽"要求的"CI 不再有脆测掩盖真 bug"。C1 另经 perfgate PASS(12 指标全在容差,热路径改动吞吐中性)双轴验证。

- [ ] **B1 交付消费者答复** — `docs/SUPPORT-LINE-3X-VS-4X-2026-07-20.md` 已写好但**尚未交出去**;对方正在为薪资数据选型,而 3.18 带 D1 缺陷且无修复版本。交付时同时问回那个只有他们能答的问题:256 KiB 悬崖形状的 3.18.x 对他们是否仍有用(取决于他们的事务大小)。
- [ ] **B1' 拍板项(用户)** — 依 B1 的回答决定是否建 3.18.x。默认不建(理由见支持线文档:带隐形尺寸悬崖的保证比明说没有更糟)。
- [x] **B2 R4 启动期不变量对账钩子** — 报告里唯一「不做则每个消费者都要重造、且各造各的错」的条目;与既有 `PREFIX.DIGEST` 配对设计。
- [x] **B3 R6 菜谱** — 「一行数据、多个派生键」端到端写进 `docs/cookbook.md`(对方甚至提出愿意贡献)。
- [x] **B4 R3 事务内索引读** — 先判定是否与 R2 同形(同一个 op-table 缺口),同形则一并补齐,不同形则单列。

- [x] **C1 跨 shard block-serve 丢元素** — **真修好了,三 reactor 确定性坐实(2026-07-23 设计 round,write-result 方案)**。① escrow(2026-07-21)关"reply 回来时 origin 记录已没"窗口。② 第二窗口:poller 路径 EOF 只设 `conn.closing`、`close_conn`(设 abandoned)延到 flush reap → reply 可在断连处理**前**到达 `origin_on_serve_resp`,旧码 deliver 进死 conn + 释放 escrow → 丢。**修法 = escrow 释放绑定 write 结果**(不在 deliver 时释放:记 `serve_confirm[conn]=target_shard`,conn output flush 干净且非 closing → `confirm_serve_delivered` 释放;conn closing/FIN 读到/写失败 → `restore_serve_on_teardown` 恢复;`closing` 是"客户端已走"的权威信号,FIN'd 客户端确定性恢复,不靠时序)。三 hook:poller `flush_conn` / uring `uring_resolve_serve` / `close_conn` teardown;全幂等(escrow_take + serve_confirm.remove)无 `*2`。顺带修预存 escrow 泄漏(旧 ack_serve 在 deliver 后跑读不到已删记录)。**验证**:`KEVY_TEST_XSHARD_HOLD_CLOSE` seam 确定性"先红后绿",**macOS-kqueue 10/10 + Linux-epoll 20/20 + Linux-uring 20/20**。**关键发现(计数器排除法)**:先前观察到的 ~1/8 残留**不是跨分片缺陷**,而是 key 与 conn 同分片(~1/N)时走**本地 BLPOP 路径**(blocked.rs),即时服务给还连着的 consumer = Redis-等价,不归此窗口管;测试改为同分片自动重试直到 `cross_shard_serves()` 确认走了跨分片再断言。实现 `block_xshard.rs`+`block_xshard_confirm.rs`+`shard_flush.rs`+`uring_io.rs`+`inbox.rs`。**ship 阻塞项真清除(多 reactor 确定性回归撑腰)。曾两度误标 `[x]` 撤回,这次有实证。**

### t5.7 — FTS arc(取代 Meili;施工顺序见 RFC)

设计:`.claude/notes/fts-arc-design-round.md` +
`.claude/rfcs/2026-07-21-fts-terminal-query-surface.md`(查询面) +
`.claude/rfcs/2026-07-21-fts-deep-structures.md`(深水区,决定 2 + 位置索引 + 有序词典)。

- [x] **步骤 1 冻结终局 MATCH 面** — 8 保留字解析即报错(指名),`e058450d`
- [x] **步骤 2 多字段 IndexSpec + sidecar v2** — v1 永久可读,`b8298c2b`;守卫多字段(仅 text 收,`de99ae50`)
- [x] **步骤 3 引擎索引加权多字段** — `apply_fields`,dl 不加权,`de99ae50`;6 新测试
- [x] **步骤 3.5 服务端 IDX.CREATE 多字段 wire 语法(`FIELDS a b [WEIGHTS ...]`)** — `4f3a1963`;扫描式解析,单 `FIELD` 路径 byte-identical,3 wire 测试(加权排序 / 默认权重 / arity 错误);`type_pos` 参数化传给 type/kind/opts 解析
- [x] **步骤 4a segment 用注入统计打分(全局 BM25 石头能力)** — `matches_scored(_, _, Some(stats))`,`matches` 是 None 糖 byte-identical;正确性测试=分片 vs 整体同分;`total_len`/`local_df` 暴露为 4b pass-1 API。Phase A 发现:query-time 两趟聚合(只需 query token 的 df)严格优于周期快照,无陈旧窗口——见 RFC §1
- [x] **步骤 4b-embedded 两趟全局 BM25** — `eea588af`+`5f735114`;`idx_match` 趟1 `text_corpus_stats` 聚合全局统计、趟2 `matches_scored`;shard-invariance 测试(ranked(1)==ranked(8) 排序+分数)CI 确认;串行单锁无死锁。教训:测试断言曾误设 d:6 top,实际 BM25 长度归一化下 d:1(短+密) top,CI 纠正
- [x] **步骤 4b-server 分布式两轮聚合** — 复用 GROUPS→AGG.FETCH 的 stateless `ExtensionReduced::Continue` 两阶段机制:pass1 `op_match` 报 stats chunk `[ST_OK][n_docs][total_len][ntok][(tok,df)*]`→`reduce_match_stats` 聚合全局 `CorpusStats`→`Continue(MATCH.SCORE argv)`;pass2 内部 verb `MATCH.SCORE`→`op_match_score` 注入全局 `matches_scored`→`reduce_match_score` 合并(与 KNN 共用 `merge_ranked`)。新增 server shard-invariance 测试(`text_global_bm25.rs`:1-shard==8-shard 排序+分数)。**本地环境瘫痪未能运行时验证(见 memory),靠 CI 裁判**。type alias 消 clippy type_complexity。perf(两轮延迟)仍需 lx64
- [x] **步骤 4b perf 定夺 — 两趟胜,代价 0.06ms**(2026-07-23)。A/B:两趟(线上)MATCH p95 28.19ms vs 单趟(4b 前行为的实验构建)28.13ms,worst-conn 两趟反更好 → **多一轮 fan-out 在噪声内**(pass 1 只搬被查询词的 `(token, df)`)。scope-decisions **决定 2 已推翻**:其"全局统计 ⇒ 写路径协调"的前提不成立——读时两趟既无协调也无陈旧窗口。`docs/text-search.md` 三处过期表述已更正(此前仍写着 "Scores are shard-local" 与 "filters/facets/highlighting 尚未构建")
- **步骤 5 位置索引** → phrase / proximity / HIGHLIGHT(施工分子步,每子步 CI-可验;positions 内存公式 lx64/textgate 在步内验)
  - [x] **5a kevy-text 存储 stone 核心** — positions 物理旁路(`Option<Positions>`,token→id→delta+varint blob),`None` 时 BM25 热路径 byte-identical;`with_positions()`/`has_positions()`;`apply_fields` 记录/撤回;`phrase_matches(phrase,limit,stats)` = 候选交集→shift-intersect 邻接验证→BM25 打分(单 token 退化为 term query,无 positions 返空非静默 OR);读/查询路径拆 `segment_query.rs` 守 500 LOC(`corpus_stats`/`select_top` 提 pub(crate) 供 phrase 复用);approx_bytes 仅在 positions 存在时加旁路项、默认公式不变。CI:邻接/顺序、off-path 空、positions-on 排序 byte-identical、更新撤回、重复 token、注入统计打分(clippy+全 workspace clippy lx64 绿)
  - [x] **5b catalog sidecar v3** — `IndexSpec.with_positions: bool`(仅 Text,`create` 守卫非 Text 报错);sidecar v3 复用"第 7 列 kind-interpreted"惯例写 `pos`(Text 与 Ann/Agg 互斥);version `bool v2`→`u8`(v1/v2/v3),v1/v2 永久可读;测试:v2-still-loads 回归 + positions 往返(pos 列存在性/plain 6 列/往返 flag)。kevy-index 27 测试 + 全 workspace clippy 绿(9 处 IndexSpec 字面量补字段)
  - [x] **5c IDX.CREATE ... WITH POSITIONS** wire 语法 + 引擎接线 — `WITH POSITIONS` 作 KIND 后的 key/value opt(`WITH`/`POSITIONS`,天然满足偶数配对),text-only 由 `Catalog::create` 守卫统一兜;`WITH` 非 POSITIONS 报错;两处 TextSegment 构造点(embedded `new_text(spec)` helper 用于 sync+FLUSHALL reset;server `index_runtime` reconcile `.then`)按 `spec.with_positions` 选 `with_positions()`。e2e(lx64 实跑绿):create WITH POSITIONS +OK + 普通 MATCH 排序不变、range 拒 positions、`WITH NONSENSE` 报错。**发现:lx64 干净盒能跑进程内 reactor e2e,paralysis 纯本地 mac**
  - [x] **5d MATCH 引号 phrase 解析** → `matches_query(text,limit,stats)`:解析 `<text>` 为 bare term(OR)+ `"a b c"` phrase clause(邻接必需),query = clause OR 并、分数 = 各匹配 clause BM25 和(phrase 仅对邻接文档贡献);无引号 → 委托 `matches_scored` byte-identical 热路径;未闭合引号 lenient;无 positions 时 phrase clause 不贡献但 bare term 仍匹配。路由:embedded `idx_match` + server `op_match_score` swap 到 `matches_query`;两轮 df 收集 tokenize 原始 text(含 phrase token)、reduce verbatim 传引号 text 到 pass-2,无需改 pass-1/reduce。CI:纯phrase==phrase_matches / 混合 OR / 无引号委托 / lenient / 无positions;e2e(lx64 真 8-shard):`"quick brown"` 只匹配邻接 p:1,不匹配远隔 p:2/逆序 p:3。+/-/field: 属其它步骤不进 5
  - [~] **5d.1 phrase 候选剪枝** — `rarest_anchor`:phrase 候选锚定**最稀有** token(所有 phrase token 必present,故稀有 token 文档集最紧),避免 head-term 首 token 扫整表;保正确性(34 测试绿),head+tail phrase 大幅加速、head+head 仍需 skip-list(未做)
  - **5e HIGHLIGHT spans**(用户 2026-07-21 授权大 epic 开工;分 5e.a stone / 5e.b wire)
    - [x] **5e.a kevy-text span 能力(stone,CI-可测)** — `tokenize_spans(text)→(token,start,end)` 带字节偏移(valid UTF-8,CJK bigram 跨 char,invalid→空;与 tokenize 一致有测试锁);`highlight_spans(key,query)→[(field_idx,[(start,end)])]` **重分析已存字段文本**(bare term 每处 + phrase 仅邻接 run),**不依赖 positions 侧map**(re-analyze highlighter,winning docs 少)。42 kevy-text 测试绿
    - [x] **5e.b wire** — un-reserve HIGHLIGHT + `MatchArgs::parse`/`parse_tail` 重构为 clause-scan(FIELDS/HIGHLIGHT 两变长 clause 各扫到下个关键字,任意序;HIGHLIGHT 空=全字段)。**追加尾部 highlights** 方案(不请求时 row 不变,契合 RFC "highlights 仅在请求时出现",避免把所有 ranked reply 重构成 map 的 blast radius):row = `[key, score, (field,value)*, [[fname, s, e, …], …]]`。server 两轮:HIGHLIGHT 线程进 pass-2 MATCH.SCORE argv → `op_match_score` 每 hit `highlight_spans`+`hit_highlight`(field_idx→name via spec) 编入 chunk → `merge_ranked` 解码 emit;`with_ready_text_segment` 闭包加 `&IndexSpec`。embedded:`idx_match_highlighted`+`emit_ranked_highlighted`。type alias `HitSpans`/`FieldSpans` 消 clippy type_complexity;LOC 拆 `args_tests.rs`/`ops_index_highlight.rs`。CI:args parse 单测 + embedded highlight 单测 + e2e 真8-shard(phrase `"quick brown" HIGHLIGHT` → body spans 4-9/10-15,无 HIGHLIGHT 时 row 不变)
  - [x] **5f lx64 textgate — 内存公式修复 + POSITIONS 模式**(两模式 lx64 全 PASS):
    - **内存公式修复(真缺陷,已修)**:positions `approx_bytes` 原低估 3.2×(lx64 ratio 0.42 FAIL)→ 改**模型化**(内层 `HashMap<u32,Vec<u8>>` = struct + pow2 RawTable×33B + 每 blob 独立堆分配 16B 对齐+header)→ lx64 复测 **ratio 0.75 PASS**。根因 = singleton min-table + tiny-blob malloc 开销(原 `blob.len()+30` 忽略)
    - **p95 provisional 上限**:term 35ms(实测 27-31ms)/ phrase 150ms(实测 102-114ms),注明 = **共享盒 regression-catcher 非精确 SLA**。精确 SLA 须干净独占盒([[feedback-kevy-bench-isolation]],perf §9 不信共享盒单跑)→ 待用户 bench-infra
  - [x] **5f.1 Positions One-inline 内存优化** — `DocBlobs::One{id,blob}`/`Many(HashMap)` mirror `Buckets::One`:hapax token(Zipf 常态,唯一 id/email/docnum 只在一文档)inline 存 blob 免 HashMap 表,第二文档起才 materialize;不 demote(同 Buckets)。`approx_bytes` 随之分 One(仅 blob 分配)/Many(pow2 RawTable+blobs)。lx64 实测:RSS 2966→**2731MiB(省 ~235MiB**,1M singleton 免 ~128B min-table+碎片),formula 2054 / **ratio 0.75 PASS**。34 kevy-text 测试绿(枚举行为等价)
  - [x] **5d.2 phrase head+head** — 三次诊断,前两次都错(2026-07-23 收口)。①条目原写「需 positional galloping」;②我改判「87% self-time 在 libc 分配器 = 分配受限」——**这个剖面抓的是分片拆除不是查询**,四个取证缺陷叠加(`strip=true` / `-- sleep N` 没约束窗口 / `pgrep|head -1` 挂到 `su` / gate python 块缓冲让标记迟到到关机),修复与自检落在 `bench/profile-textgate.sh`;③干净剖面(`drop_glue: 0`)给出真相:**82% 在内存会计** —— 跨分片 pass-1 只要一个文档数,却调 `stats()` 顺带遍历全索引算 `approx_bytes` 再扔掉。加 `TextSegment::docs()` 改三处调用点后 **phrase p95 87.4→23.5ms(-73%,3.7×)**,三轮交替配对区间相距近四倍。此前消 `Positions::get` 分配的 **-6.2%** 仍然有效(`9f85de2c`),只是量级远小于当时的判断。**galloping 仍未验**,但在 23.5ms 且检索仅占 4.5% CPU 的前提下已非瓶颈,故勾掉而非留半开
- **步骤 6 prefix / TYPO**(设计决断:不改 postings HashMap 结构=保热路径 O(1)/零 O(n²) build,prefix 用 scan;ordered-structure/FST 是 prefix perf 的 future 优化,RFC 的 sorted-vec 有 O(n²) build 问题 RFC 自己预见会逼 FST)
  - [x] **6.a kevy-text prefix 能力(stone,CI-可验)** — `matches_prefix(prefix,limit,stats)`:ASCII-lowercase 前缀 → scan postings keys 过滤前缀 → 展开词 OR 打分(复用 add_term)→ select_top。零 postings 结构改/零写路径改/零热路径风险。CI:前缀展开(qui→quick/quiet 非 quality)、大小写不敏感+narrowing、空/无匹配/limit0、前缀=完整词。46 kevy-text 测试绿
  - [x] **6.b wire** — 语法定案 = **`word*`**(MATCH text 内,parse_clauses word-split 检测尾 `*`,与 phrase/term 混合任意 OR)。`matches_query` 加 prefix clause(add_prefix=expand_prefix+add_term OR);highlight 也高亮前缀匹配 token。**全局 df prefix**:`query_df_terms(&self,text)`(bare+phrase+prefix 展开,per-shard)接入 server `op_match`(闭包内 per-shard 展开报 df)+ `reduce_match_stats`(改 `entry().or_insert` 接受所有上报 token 含展开词)+ embedded `text_corpus_stats`(签名 q_tokens→text,per-shard query_df_terms 累加 df)。非 prefix 查询 byte-compatible。type alias `Clauses` 消 clippy。CI:语法==primitive/混合 OR/prefix highlight;e2e 真8-shard `qui*`→quick+quiet 非 slow + 全局 df。
  - [x] **6.c lx64 prefix p95** — textgate 加 `PREFIX=1` 模式(`word*` 查询 mix)。lx64 实测 **p95 94ms**(<200 provisional PASS,内存 ratio 0.56 PASS)@ **~1M-term 压力词典**(1M doc-marker 灌进)。**定夺:scan 可用但 O(dictionary)**——真实 ~200k-term 语料约快 5×(~20ms);FST 让 prefix O(log n)对超大词典加速 = **future 优化非阻塞**(scan 是合理首版)
  - [x] **6.d TYPO 端到端** — `TYPO 0|1|2` clause(un-reserve;`AUTO` 在冻结面但未建 → **明确报错非静默**)。stone:`edit.rs` bounded Levenshtein(两行 DP + 行内全超预算即弃 + 先按长度差拒;**字节级**=ASCII/Latin 准确,多字节字符按其字节计,已声明非静默近似)+ `expand_typo`/`add_typo` + `matches_query_typo`/`query_df_terms_typo`(sugar 保留旧 3 参签名,零调用点churn)。**仅 bare term 被 fuzz**(phrase 要精确邻接、prefix 本就模糊)。wire:MatchArgs/parse_match_score 带 typo、reduce_match_stats 线程进 pass-2 argv、op_match 用 `query_df_terms_typo`(展开词全局 df)、op_match_score 用 `matches_query_typo`;embedded Tail.typo + `match_clauses` 门控 + idx_match_highlighted/text_corpus_stats 带 typo。CI:57 kevy-text(含 3 edit + 5 typo)+ args parse(0/1/2、AUTO/3/x 报错、与 FIELDS 共存)+ e2e 真8-shard(`quik` 精确查不到、`TYPO 1` 命中 quick、turtle 不误命中、`TYPO AUTO` 报错)
  - [x] **6.e OFFSET 端到端** — `OFFSET n` clause(un-reserve)。**语义 = 跳过 MERGED 排名的前 n 条**(非每分片):pass-2 各 shard 取 `limit+offset`(分片无法知道自己哪些命中能活过合并),reduce/embedded 排序后 `drain(..offset)` 再 `truncate(limit)`;KNN 复用 merge_ranked 传 0。LOC 债:`args.rs` 拆 `args_match_score.rs`(pass-2 解析)、`MatchArgs::parse`/embedded `parse_tail` 各抽 clause-dispatch 助手。CI:args parse(LIMIT+OFFSET 共存、非数字报错)+ e2e 真8-shard(两页不重叠、超尾返回空非报错)

- **步骤 7 IN(字段限定)**(设计决断:**不做"匹配后按字段过滤"的近似**——那样打分仍是合并 tf + 全文档长度归一。改为 positions 已验证的**物理旁路**模式存逐字段分解,默认路径热路径不变、单字段索引零成本)
  - [x] **7.a kevy-text 逐字段 postings(stone)** — `fields.rs`:token→(doc→varint 打包的**已加权**逐字段 tf,复用 `docblobs.rs` 的 One-inline+Many)+ 逐字段语料总长 + **扁平 stride** 逐字段文档长度(每文档零分配)。存**已加权**而非原始 tf 是关键:全字段求和==合并 posting(权重按索引时),测试 `scoping_to_every_field_equals_the_unscoped_query` 直接比**分数**而非仅排名。df **走一次遍历精确计数**(逐字段 df 相加会把同时命中两字段的文档算两次)。短语也限定:位置是拼接字段上的序号,故限定短语必须落在**单个**目标字段的序号区间内——顺带拒绝"跨字段边界看起来相邻"的伪短语。结构:`DocBlobs`+varint 提到 `docblobs.rs` 两旁路共用;子句累加半边提到 `segment_scope.rs` 的 `Scope` 上下文,**一套子句引擎服务两条路径**;`QueryOpts` 取代 (stats,typo,fields) 参数尾。71 kevy-text 测试绿
  - [x] **7.b IN wire 端到端** — `IN title body` 离开 NOT_YET。两轮协议**形状不变**:限定时 pass-1 上报的 n_docs/total_len/df 就是字段限定值,reduce 照常求和,pass-2 注入的 avgdl 即"这些字段的平均长度"。**过线的是名字不是位置**(名→位置要 spec,只有持有 segment 的分片有),故**未声明字段名 = 报错**(新 ST_NOFIELD 状态,载荷带冒犯名+已声明名):静默返回空会让字段名打错与"语料无命中"不可区分。暴露并修掉两处重复真相:**embedded 有自己的 segment 构造点**(给多字段 spec 建了单字段 segment → IN 静默匹配全部;embedded 测试抓到)、**pass-2 argv 手抄了第二份子句循环**(现返回同一 `MatchArgs` 跑同一 `apply_clause`)。embedded 补 `idx_create_text`(此前进程内无法建多字段文本索引,IN 无从表达)+ `MatchOpts`。lx64:clippy 全绿 / kevy-text 71 / args 12 / embedded 243 / **e2e 18**(含真8-shard 字段限定+未声明字段报错);locgate 顺手拆 4 文件 3 函数
  - [x] **7.c textgate FIELDS 模式** — 新内存项必须**校准**非断言。1M 文档双字段:**内存公式 ratio 0.75**(0.5–1.5 PASS,**首跑即准**——positions 那次首版仅解释 0.42× 需重建模型,本次复用其修正后的模型)+ 字段限定 p95 **123.67ms**(<250 provisional)。限定查询 p95 高于不限定的 27ms 且**如实说明原因**:impact bucket 是按**合并** tf 排序的,不是被打分的那个,故限定走逐文档遍历无剪枝;恢复剪枝=给逐字段 postings 也做 impact bucket,是真选项(代价是内存),留待权衡
- **步骤 8 doc values epic**(设计见 `.claude/rfcs/2026-07-22-fts-doc-values.md`;RFC 原断言"四条 clause 各是既有结构之上一层"**被推翻** —— 既有一切是 term→documents,四条都要 document→自身的值,故是**一个缺失结构+四层薄壳**)
  - [x] **8.a doc values 石头** — `docvalues.rs` 列式存储(id→值,扁平 stride);**23 字节以内内联**(槽位无论装 Vec 还是内联都是 32B,故内联零额外空间 + 省掉每文档每字段一次分配,有测试钉住尺寸);谓词在 `select_top` 施加=**每候选恰好测一次**;**带 filter 主动放弃 MaxScore**(剪枝阈值按未过滤第 k 名算,若被拒的正是领先者,合格文档可能从未被累加=丢失而非排错);**缺值不通过**。测试布置成顺序错必炸(合格五篇=最差五篇)
  - [x] **8.b VALUES 声明** — `IDX.CREATE … VALUES f… [TYPES t…]`(text-only);sidecar v3→v4(文本第7列=逗号分隔,首 token `pos`/`-`,**只有位置时写出的仍是 v3 的 `pos`**);**类型是声明的不是猜的**(数值范围按字典序比是静默错误;按"两边都能 parse 成数"决定则答案随数据而变);`IndexSpec::read_row` **消掉 server/embedded 两份"按 spec 读行"的重复**(闭包供给 hget,kevy-index 保持零依赖)
  - [x] **8.c FILTER 端到端 + 8.c.2 embedded 对等** — 语法=既有 `RANGE`/`EQ`;**FILTER 不动语料统计**(非打分谓词;IN 才动=两条正交轴);比较下沉 `kevy-index::ValueTest`(EQ=退化区间 `[v,v]`,两 crate 共用);三种错法三种报错(字段未存储/bound 类型不符/存值无法 coerce 则不通过);`ST_NOFIELD`→`ST_CLAUSE`(载荷即完整解释)。e2e 19 真8-shard
  - [x] **8.d textgate VALUES 模式** — **同日同盒 baseline 对照**:默认 691MiB/1176MiB ratio 0.59,VALUES 723MiB/1227MiB ratio **0.59**(公式增 32MiB、RSS 增 51MiB、**比值一动不动**=新项精度与既有模型同级)。filtered p95 46.17ms vs 未过滤 26.82ms=放弃 MaxScore 的代价,已实测非假设

  - [x] **8.e SORT / 8.f DISTINCT / 8.g FACET —— 三条全落且**跨分片精确**(RFC Addendum 2/3 记下了对本 RFC step 5 的更正:让分数 fan-out 精确的 k 路归并论证**没提分数**,对分片一致同意的任何全序都成立)
    - **SORT**:分片**按排序键**选页(不是按分数选完再重排——那会丢掉排名靠后但键上该赢的文档);次序只有一个定义 `kevy_text::sorted_order`,分片与 origin 共用;回传顺序保持编码 `kevy_index::order_key`(i64 翻符号位/f64 IEEE 全序/str 原样),origin 不需懂类型;**缺值两向都排最后**
    - **DISTINCT**:折叠在**选择期**(否则页会缩水);精确性证明在 RFC;身份=coerce 后的值(复用 order_key);**无值文档自成一组**
    - **FACET**:在截断**之前**计数(LIMIT-1 也报全部桶);`FILTER` 减计数、**`DISTINCT` 不减**;按身份求和、按语料里真实出现过的标签上报;回复**追加一个末元素**,无 FACET 时逐字节不变
    - **NOT_YET 表清空** —— v4 冻结的终端查询面全部可执行;保留机制本身(新加子句仍会具名报错而非静默忽略)
  - [x] **8.h textgate ORDER 模式** — 纯 MATCH 26.82ms / FILTER 46.17ms / **SORT·DISTINCT 55.92ms**(<400 provisional)。约为剪枝路径两倍,是"按分数以外的东西选择就必须看每个候选"的诚实形状;此前只在 commit 里声明过代价,现在有数
- [x] textgate 在步骤 4/5 内重录基线 — 5f 已做(POSITIONS 模式 ratio 0.75);此后每个改内存公式的步骤都在步内重录:7.c FIELDS 0.75、8.d VALUES 0.59(同日 baseline 对照)

### t5 — 总线收尾(渠道除外)
- [ ] lx64 post-fix arena 复测(悬案)→ README 基准表解冻
- [ ] CHANGELOG 4.0.0 补总线各 train;五轴终审(ship 挪到 t6 渠道后)

### t6 — 渠道真发行 + ship(用户拍板 2026-07-14 放最后;名字勘定同日:brew formula `kevy` 404 可用、npm 裸名 `kevy` 与 `expo-kevy` 可用、nuget `Kevy` 0 hits 可用、apt 自建仓库包名 `kevy`;npm scope 统一 **@goliapkg**(两 scope 均未发布过,零迁移),包机制文件已全部切换)
- [ ] 拍板后:brew tap 建 repo + formula 发布;apt 仓库上线 t01;npm 平台分包发布(kevy-bin + kevy-node + expo-kevy);NuGet push;kevy-go repo 剥离 + tag
- [ ] 发行后三渠道真装 smoke 重跑(脚本已有)+ site 安装页六语言
- [ ] README 六语言矩阵 + llms.txt 同步
- [ ] 五轴终审 → ship **v4.0.0**(tag 前 CI 真绿 + 用户验收)
