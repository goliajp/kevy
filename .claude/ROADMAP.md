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
- [x] embedded bench —— RFC + 四语言 harness + 两攻击面全落地(2026-07-23~24)。**明细见** `bench/EMBEDDED-LEDGER.md`(四轨 vs 五引擎 + 跨轨综述)/ RFC `2026-07-23-v4-embedded-bench.md`(对手·轴·公平性,含 LiteDB→LMDB 更正)/ `2026-07-24-embedded-bulk-write-ceiling.md`(引擎 bulk 天花板决策)/ CHANGELOG 4.0.0。**核心**:① kevy_get_shared 零拷贝读**反超 LMDB 2-9×**;② `GetView`(Go/C#)闭合大值 GET 绑定输(64KB 62×→反超,vs badger/LMDB copy 胜 52-78×);③ `kevy_set_many`/`SetMany`/`setMany`(C/Go/C#/Node)批量写 —— **仅 Go cgo(昂贵 crossing)受益 147×→2.7×**,C#/Node/C 引擎受限 ⇒ 证 bulk 缺口是引擎级(per-op durable AOF frame,**决策保留 per-op durability**)。**dispatch_oracle 修复**(embedded IDX.CREATE FTS parity,`7dec633f`)。**待**:lx64 SLA 定值(dev-host 相对位次已入账、方向性成立;需 push 或盒部署)。
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

### t5.8 — capacity arc(容量时代:透明分层 × 虚拟 RDS 视图,融合;**v4 发布重要功能,用户拍板 2026-07-24 列入 v4**)

主设计 = `.claude/rfcs/2026-07-24-v5-capacity-arc.md`(验收标准 A-D 全 gate 映射 / 核心设计 D0-D4 / 16 项交互矩阵 / **§7 预决策清单 —— 全部旋钮已定死,执行期零决策**;文件名带 v5 为历史命名,内容已改标 SHIPS IN v4.0.0)。要点:主表内 `Value::Cold` stub + 两段式 funnel;vlog 可弃(AOF 唯一 durability;持久化从 pinned vlog 流式不 promote;replay 内联下沉 —— 两条 launch-blocking);demote/promote_in_place 零事件原语;统一水位 = budget·19/20 − index_bytes − stub_bytes;G1 泛化为融合基石;TABLE.* 声明期编译;kevy-sql 引擎外。默认值全在 RFC §7(tiering 默认 OFF / auto=0.70 / tiered-lru / min_spill=64B / 二次访问才 promote / embedded max_spill_value=256KiB / spill 批 32 / vlog 轮转 256MiB·活率<0.5 压实 / TABLE.* / kevy-sql 独立 crate + kevy-cli sql / envelope 于 lx64,10× gate·100× stretch)。G4 视图 FILTER、集合 spill、embedded 放锁 dance、冷读全异步、kvrocks 竞对 = **明确不在 v4**(RFC §7 末,post-v4 具名)。

- [x] **T0 门先行**(2026-07-24,分支 `feature/v4-capacity`):RFC §8 验证映射表(A1-D4 每条 → gate/断言归属);`bench/tiergate.sh`(12 线,标 train 归属,RED exit 1)+ `bench/tablegate.sh`(5 线,RED)先红;透明套件 `crates/kevy-embedded/tests/tier_transparency.rs`(37 用例含全部 B9 专项 `// B9:` 标记 = T3 的 FORCE_DEMOTE 插入点;untiered 自检绿,tiered #[ignore] PENDING(T3))
- [x] **T1 kevy-vlog 石头**(2026-07-24,`a9519d94`):`crates/kevy-vlog`(lib 326 LOC + crc32c 镜像 73):append/read_at/rotate/compact(CompactOwner trait 单借用,is_live→append→moved;全死文件零扫描)+ 每记录 CRC32C + pin = Arc<VlogFile>+delete_on_drop(最后持有者落时 unlink)+ epoch + open 即弃(disposable 契约有测)。**记录携 key(自描述压实);值序列化(含 hash 格式/field-TTL)按依赖方向归 T3 kevy-store 侧**(纯字节石头不能依赖 kevy-persist——成环)。10/10 单测(含 2000 步 splitmix churn)+ **60s fuzz 78,425 runs 零 crash**(vlog_churn 入 fuzz.yml)+ bench 入账(append 2.1-27µs/op 至 2.4GB/s,read_at 0.64-14µs = B2 预算内)+ clippy/locgate 绿;release.yml 双发布链已登记
- [x] **T2 G1 引擎泛化**(2026-07-24,`bb91b407`,33 文件 +2788/−428):VALUES/FILTER/SORT/DISTINCT/FACET/OFFSET 落地 Range|Unique。kevy-index:`rowvalues.rs`(row-key→值列,≤23B inline)+ `segment_claused.rs`(单 pass FILTER 流式/FACET 全集计数/DISTINCT 选择期收敛/SORT 全集排序 + `merge_claused` 泛型 origin 合并)+ `values: Option<RowValues>` 物理旁路;guard 单点放开(`text|range|unique`,ann/agg 仍拒——顺修 embedded 静默丢弃)。server:`args_scalar/query_claused/reduce claused`,**FILTER 可与 CURSOR 组合(旧 chunk/reduce 原样),SORT|DISTINCT|FACET|OFFSET×CURSOR 具名拒**;IDX.COUNT 带子句 BADARGS。embedded 全 wire 平权(错误次序对齐 server)。**sidecar v5 = 写能表达数据的最老 header**(无新能力的 store 磁盘字节不动,v1-v5 全读)。**A5 三层证明**:sidecar 字节断言 / Option 旁路 never-taken / VALUES 索引纯 RANGE reply 与无 VALUES 孪生逐字节等。语义精确复刻 text(missing 双向排最后/DISTINCT order_key 强转身份/FACET 截断前全集+FILTER 减 DISTINCT 不减)。验证:kevy-index 60 + kevy 84 target + embedded 247+12 + **oracle 2/2(+13 案例+28 条独立 server 钉死)** + clippy/locgate + workspace 158 target 全绿(本树独立复验,陈旧 server binary 假红一次,重建后真绿)
- [x] **T3 store 核心**(2026-07-24,`700e9f9f`):`Value::Cold(ColdRef)` 22B(u64+3×u32+tag+touched,≤24B payload,32B/48B 断言编译过)+ 两段式 funnel 全挂点(读闸门 get 族/hash 读族;&self 共享道 pread 永不 promote;写解析 tag 匹配→promote、不匹配→WRONGTYPE 零 pread;stub 原生路径 SET 裸探测/NX/XX/rename 不读值/flushall/type_tag)+ `demote/promote_in_place`(零事件/保 hfttl+lru/expires 不动)+ evict 分叉(采样跳 Cold+非可溢类型,spill 批 32+tick 续)+ 二次访问 promote 门 + `debug_force_demote` seam + 最小 config(embedded with_tier_budget + mem:// 具名拒;server KEVY_TIER_BUDGET env)+ tier_codec(bulk/hash 含 SmallHashInline)。**矩阵外抓 4 真坑并修**:RENAME×compaction 前向指针(TierState.renames 四点维护)/ COPY stub 别名(clone 改 peek 物化)/ 共享锁道冷键永不 promote(tiering on→独占道)/ maxmemory=0 时钟不走针(clock_on)。**B9 透明套件转绿**(tiered-vs-untiered 3/3,10 markers 真 force-demote,B12 零事件+计数器断言)。验证(本树):kevy-store 125 + transparency 3/3 + oracle 2/2 + workspace 158 target + clippy/locgate + no_std/wasm 构建全绿。A1/A2 perfgate 线待 lx64(T9);persist 冷臂 debug-assert 占位(T4 接管)
- [x] **T4 持久化流式集成**(2026-07-24,`3746c0cc`,17 文件 +768/−38):**pins 住进 SnapshotView 本体**(`pins: Vec<Arc<VlogFile>>`,collect_snapshot 捕获)→ COW rewrite/复制 ship/SAVE 零改动继承;Multi 多 view wrapper per-view 物化(多 shard file_id 冲突由构造正确);codec 单实现私有于 kevy-store(`materialize_cold` 两个 pub 方法,Cold 臂物化后递归既有 emit 臂,drift-proof,内存界=单值);**Cold 臂从 debug-skip 改 loud io::Error(release 不可能再静默丢冷值)**。replay 内联下沉全挂点:embedded replay/reshard、server poller+uring boot、server reshard(tier scratch dirs)、snapshot_read(每 1024 + EOF)。**超 spec 抓 2 真坑**:①`load_str` 令 snapshot 载入字符串永不可 spill(B11 首跑 0 demotions 暴露;修=走 pick_value_for_set_owned,载入编码与 live SET 对齐)②测试预算须高于 stub 地板(96B/键模型实测吻合)。B10(rewrite/snapshot 双路,零 promote 断言+field-TTL 存活)+ B11(boot 超预算 1500 键逐字节 + reshard 冷集)4/4;pinned view 真 file-retirement 测试(compact 退役后 view 仍读、drop 才 unlink)。验证(本树):persist/store/embedded/oracle/transparency/locgate 全绿。诚实边界:真 replica e2e 冷例未上(unit 级精确管道覆盖);server tiered reshard 无独立 harness(embedded 同构 e2e 有)
- [x] **T5 统一预算 + auto 探测 + INFO**(2026-07-25,`fcdc65b6`,45 文件 +1729/−102):统一水位 `budget·19/20 − reserved − stub`(饱和到 0 在 INFO 可见);stub_bytes/cold_bytes 全路径增量维护(demote/promote/DEL/overwrite/RENAME heap 差/FLUSHALL);reserved_bytes 双侧 tick 喂入(server commands.rs tier_tick + embedded shard tier_tick_upkeep,scalar/text/ann/agg/view 全 kind 求和);**预算语义修正:整店级按 shard 均分**(T3 per-shard 原值在 auto 下会 ×N,已修);IDX.CREATE floor 拒绝双侧同 wire 字节(oracle 不加 case——裁判 plain store 触发不了,embedded typed+wire 双断言 + 本机 server live smoke 真拒);auto 探测 = min(cgroup v2 memory.max, MemAvailable)×0.70 / macOS sysctlbyname 手绑(kevy-sys mem.rs,fixture 单测+真 sysctl);config 三形态(auto/N%/bytes)TOML/CLI/env/CONFIG GET-SET + embedded builder 平权;INFO `# Tiering` 13 字段双侧一致、off 时整段缺席(untiered 字节不变,transparency 复跑绿);memgate B7 行 + tiergate L8 断言体落地(PENDING 至 lx64,TIERGATE_RUN_L8=1 翻转)。live smoke:1MB 预算 300×8KB 收敛,stub_bytes=185×96 精确,CONFIG SET 一 tick 生效。验证(本树):sys 6/config 103/store 133/embedded 全套(budget 7+persistence 4+transparency 3)/oracle 2/2/locgate 全绿;wasm32+aarch64-linux 交叉 check 净
- [x] **T6 hydration 冷批量 + no-promote peek 推广**(2026-07-25,`20efc16b`,35 文件 +1992/−592):`peek_hash_fields/peek_hash_rows/peek_scope` + `ColdBatchReader` trait(ColdRead 携 pin,compaction 安全);批结构共享(collect→(file_id,offset) 排序→pin→批读→verify_image→每记录单 decode→原序回填),两后端只差 read_batch;**uring 批读因 kevy forbid(unsafe) 下沉为 kevy-uring 安全原语 `read_file_batch`**(per-shard 次级 64-entry ring,thread_local 懒建,一次 enter submit-and-wait,SQ 满分 chunk;不碰 socket ring;全异步冷读按 §7 留 post-v4);poller/embedded = SyncColdRead 有序 read_at。采用点全列:server 六条 hydration 路径(MATCH/KNN/HYBRID/COMPOSE/scalar/claused)+ op_hydrate + backfill 四 kind(row_apply.rs)+ IDX.VERIFY + PREFIX.DIGEST + scope_move + embedded 四 kind 同步/digest。**顺带:VlogFile::read 2 pread→1**。计数器 peek_preads_total/batch_submissions_total 入 INFO。新 server 级 tiered e2e(in-process 8-shard 双子:digest 等值零 promote/冷表 backfill+VERIFY 双端一致/FIELDS 字节一致/**preads==cold rows 非 rows×fields**/submissions≤shards/全程 promotions==0)。验证(本树):store 140/vlog 13/index_e2e 23/tier_hydration 1/embedded 三套 14/oracle 2/2/locgate + workspace 2156 passed 0 failed;wasm+aarch64-linux check 净。诚实边界:uring 路径仅编译/clippy 级(macOS 主机),真机跑 = lx64/CI(cfg(linux) 天然 gate)
- [x] **T7 TABLE.* 声明层**(2026-07-25,`b6bfd96a`,10 新文件+~38 改动):**composite Range 引擎**(kevy-index composite.rs:order-preserving 编码——i64 翻符/f64 total-order 定宽 8B、str 0x00 转义+双零终结、DESC 整分量取反、MAX_STR_COMPONENT=255 保 bounds 精确;300 随机元组暴力对拍;CompositeCol 携类型——否则 sidecar 重载即 drift,v6 sidecar 写最老 header 律续)+ `composite_bounds` 等值前缀+range = 经典复合 btree;**TableSpec/compile/TableCatalog/wire 语法全在 kevy-index 单实现**(parity 漂移根治),server cmd_table + embedded ops_table 薄壳;`IDX.QUERY … WHERE`(前导前缀律,违者具名错,永不扫描;与 T2 子句全正交);Agg 具名拒。**C1 conformance R1-R11 全映射**(table_e2e 11 测,含 WATCH/MULTI 乐观锁 wire、鸽笼保证的 verify-not-enforce、软删+FILTER)+ C2(VERIFY fsck+投毒抓 mismatch+oracle 第三测 TABLE 面字节平权)+ C6/D2(冷表 index-only preads==0/promotions==0)。**tablegate 5/5 翻转即 PASS**;perfgate 三 table 线 = 显式 SKIP-with-notice(四处环全处理,lx64 录基线);aigate PASS(COMMAND COUNT 188 含四 TABLE verb)。计划外:embedded tables 注册表生命周期 bug(kevy-client mem:// 复活测试抓出,移 DropGuard)。验证(本树):kevy-index 74/table_e2e 11/tier_hydration 1/oracle 3/3/tablegate PASS/locgate/workspace 162 套件全绿
- [x] **T8 kevy-sql 编译器**(2026-07-25):独立 crate(手写 lexer+递归下降,运行时纯 std 0-dep;kevy-index 仅 dev-dep 作 round-trip 网——每条编译出的 TABLE.DECLARE argv 过引擎自己的 `parse_table_declare`+`compile_table`,语法不可能漂移);子集 v1 全实现(CREATE TABLE 粗类型映射 21 型→i64|f64|str 逐列诚实 note;CREATE [UNIQUE] INDEX 单列→Range/Unique + PG `INCLUDE`→VALUES、多列→ORDERPATH 自动名 `a_b`;单表 CREATE VIEW:常量→VIEW.CREATE 树、参数化/带子句→query card = IDX.QUERY argv 模板 + `$N` 槽 + 机器可读 `#@card/#@param/#@argv` 段);**编译器不 plan**:确定性四级路径匹配(常量引擎视图→单列直驱→ORDERPATH 前导前缀→驱动+INCLUDE 残余 FILTER),不中即具名报缺什么声明("add: CREATE INDEX ON t (dept, age)");其余 SQL 全部具名 teaching 拒(JOIN/子查询/OR/GROUP BY/表达式/NOT NULL/严格 `>` 非整数… 均 line:col + 指配方);`kevy-cli sql compile [--apply --url]`(--apply 逐条打回执、错误回执即停非零退出);cookbook §22 "Porting a PG/MySQL schema"(docs/examples/shop.sql 真 schema 端到端 + 真 JOIN 拒错,smoke 110 条全绿);fuzz target sql_compile 入 fuzz.yml(本机 90s 8.1M runs 零 crash);release.yml 双链注册。验证:kevy-sql 35 测 + kevy-cli sql e2e 4 测(真 server apply + card 查询 + 二次 apply 停错)+ oracle 3/3 + locgate/clippy/cookbook-smoke PASS
- [x] **T9 容量终账 — 全量 envelope 真实 NVMe 全绿(2026-07-25 深夜)**。lx64 `CAPACITY_SCALE=full` EXIT=0 "PASS (full scale)":L2/L5/L6/L12/L13/L14 全 PASS —— C4 56µs·C5 429µs·hydration 465µs(1000/5000/10000 目标)/ B2 冷读 hash 145µs·scalar 105µs / D4 114µs→173µs / B5 vlog amp 1.27x / **B6 data:RAM 10.0x·used_peak 2.04GB≤2.26GB cap**。**攻坚史(perf 方法论翻盘 4 次)**:root perf(非 root 被 `perf_event_paranoid=3` 静默拦空误导两轮)定位 74% CPU 在 `RowValues::approx_bytes` —— `tier_tick` 每 100ms 经 `Segment::stats()` 对 VALUES 侧信道遍历全 10M 行 → 加运行计数器 O(1)(`91c89e7b`,c4 65ms→79µs);D4 门 µs 尺度重校准(max(2×,+1ms));B6 三连修 = ①tmpfs 守卫(数据目录必须真盘,否则 vlog 在 RAM 分层失效+StorageFull panic)②字节界 load(避开正交的既有 reactor 深-pipeline 流控 wedge,`PERF-FINDING-…-uring-deep-pipeline-wedge.md`)③**B8 校准到逻辑界**(用户拍板:used_memory 为容量契约=Redis maxmemory 语义,RSS 碎片 1.34x 上报不 gate;glibc brk 堆 4KiB churn 碎片实测 malloc_trim/arena 均救不回,`PERF-FINDING-…-b6-rss-glibc-fragmentation.md`)。三 finding doc 存档。**本地半已完(2026-07-25,`44a4de96`+`8f380198`)**:本地半:`docs/tiering.md`(330 行,预算模型/透明语义/INFO 全字段/诚实 perf 期望+Gate status 账本段)+ `docs/tables.md`(251 行)+ UPGRADING/CHANGELOG capacity 段 + README 三语 headline + **`bench/capacity-envelope.sh` turnkey 运行器**(D1/B6/B2/L5 四相 + tiny-scale 本机 harness-proof 实跑 exit 0:index-only 0 冷读/每行一 pread 含 shard fan-out 界发现/14 op 冷键 sweep/amp 1.57x;tiny 结果**拒绝翻转 tiergate**——SCALE=full 守卫)+ tiergate 六线接 envelope 结果文件。**两处诚实偏离转正**:①`max_spill_value` 旋钮(§7 预决策)被 docs pass 发现根本没实现 → 已补落(`8f380198`:store cap 双检查点 + embedded 默认 256KiB + builder + reshard temp + 测试)②crashgate-tiered 表述撤回(实存的是 tier_persistence B10/B11)。顺带:docs-parity 再生成(T7 后未再生)+ gen_command_pages table 组标签 + tablegate BSD mktemp 可移植性修复(假红根因)。**lx64 半(推送后 turnkey)**:`CAPACITY_SCALE=full KEVY_BIN=… bash bench/capacity-envelope.sh`(D1 10M×1KiB@3GB / B6 5M×4KiB@2GB / B2 p99 / L5 amp)→ `TIERGATE_RUN_ENVELOPE=1` 翻六线 + `TIERGATE_RUN_L8=1` 翻 L8 + perfgate 录 A1/A2/tiered_hotset/table_* 基线 + memgate B7 行实跑 → 全绿后 T9 收口 + 五轴终审

### t5 — 总线收尾(渠道除外)
- [ ] lx64 post-fix arena 复测(悬案)→ README 基准表解冻
- [ ] CHANGELOG 4.0.0 补总线各 train(**含 t5.8 capacity arc 头条**);五轴终审(ship 挪到 t6 渠道后,t6 在 t5.8 完成之后)

### t6 — 渠道真发行 + ship(用户拍板 2026-07-14 放最后;名字勘定同日:brew formula `kevy` 404 可用、npm 裸名 `kevy` 与 `expo-kevy` 可用、nuget `Kevy` 0 hits 可用、apt 自建仓库包名 `kevy`;npm scope 统一 **@goliapkg**(两 scope 均未发布过,零迁移),包机制文件已全部切换)
- [~] 渠道:**crates.io 34 crates 全部 4.0.0 已发(含 kevy-vlog + kevy-sql,t5.8 新增两 crate 已并入发布链)**;**GitHub Release v4.0.0 三平台二进制 + sha256 已发**;**npm `@goliapkg/kevy` 4.0.1 已发**。**未做**:brew tap 建 repo + formula;apt 仓库上线 t01;npm 平台分包(kevy-bin + kevy-node + expo-kevy);NuGet push;kevy-go repo 剥离 + tag
- [x] **发行后三渠道真装 smoke 通过(2026-07-26)**:crates.io `cargo install kevy --version 4.0.0` 从发布 crate 编译成功→二进制报 `kevy 4.0.0`、PING/SET/GET 正确;GitHub Release macOS 二进制 sha256 校验 OK + 对话正常;npm `@goliapkg/kevy` 装出 4.0.1(6 文件)、wasm 引擎 set/get/del 往返正确(Node 下默认 `fetch(file://)` 不适用属浏览器向设计,传字节即可)。**site 安装页六语言完成**:三语首页「把客户端指过来」一步具名 node-redis/ioredis·go-redis·StackExchange.Redis·redis-py·hiredis 并说明 raw 通道 + CI 六客户端梯子,已部署 kevy.golia.jp
- [x] **README 六语言矩阵 + llms.txt 同步(2026-07-26)**:三语 README 的 Install 段改为先讲「连 kevy 不需要装 kevy 包」+ 六语言(Node/Go/.NET/Python/C/Rust)客户端与 raw 通道对照表,并诚实标注各语言原生嵌入绑定尚未进 registry(实查 npm/PyPI/NuGet 得证)。顺带修掉两类真错误:**错 npm scope `@goliajp/` → `@goliapkg/` 共 51 处**(含已发布 npm 包自带 README,已发 4.0.1 修正)、**中文 README 23 处指向不存在的 `docs/zh-CN/`**(12 条死链,三 README 现均为 0)。llms.txt/llms-full.txt 已随站点重生成
- [x] **v4.0.0 已 ship(2026-07-26,用户拍板「发布吧」)**:tag 打在 develop(惯例;master 停滞不参与)。发布途中排掉三个真阻塞——① 首次 tag 被 verify 门拦下(`serve_counters` 是 `cfg(debug_assertions)` 导出而测试无条件用它 → `cargo test --release` 编译失败;该命令只在发布流水线跑,CI 绿也照样爆;修 = 测试整体 gate 在 debug)② 5 个 v4 新 crate 未入发布链(tmpdir/ranktree 必须发,三个 door crate 标 publish=false)③ npm 包版本从不 bump 且 scope 写错 → 改为版本从 tag 推导、scope 从 package.json 读

---

# v4.1 — dogfood 体系修复 arc(mailrs 反馈,用户拍板 2026-07-27:看体系不看单点,no defer)

**输入**:`mailrs/.claude/notes/kevy-v4-dogfood-feedback-2026-07-26.md`(17 findings,两次 prod 事故)。
**设计**:`.claude/rfcs/2026-07-27-v4.1-dogfood-systemic.md`(五个体系诊断 D1-D5 → 八个 train V1-V8;
**自查已在代码坐实,三处比报告更糟**:embedded VERIFY 是匿名数组元组 / typed 门面从不 validate 直通
compile 的 expect / tick 固定 CPU 的主项是每 tick 全量走文本索引 stats 而非 key 扫描)。

五个诊断一句话:**D1** typed 门面是没人测的双胞胎(RESP 面有全套 gate,embedded 面缺类型、跳准入、匿名元组,
CI 无消费者位置链接);**D2** 准入分裂在两张脸(只有 wire 调 validate → 下游 expect = 别人 boot 路径的 panic);
**D3** 周期任务无 idle 态(幂等≠收敛,300–500× 空转 CPU 让旗舰功能被关);**D4** 报表混时间语义 +
缺解释意外的计数器;**D5** 迁移知识只在 mailrs 笔记里(八课 + "可验证性"这个最强卖点都不在 docs)。

## 线性 checklist(V1-V8;TABLE 线 V1→V4、tiering 线 V5→V6、paper 线 V7→V8 三线独立,线内有序)

### V1 — 门面平权 + facadegate(D1)✅(`8d510fea`)
- [x] re-export TableSpec/TableIndex/OrderPath + `Value`(第二例同类缺口);VERIFY 匿名元组 → 具名 struct;旧 alias deprecated shim
- [x] **facadegate**:工作区外消费者 crate(独立 lockfile,只 path-dep 两个门面),纯门面 import 走全公开功能族(KV/pubsub/durability/index/view/text/table 全链/tiering 配置);CI job。F7 从结构上不可再犯

### V2 — 单一准入权威,声明路径 panic-free(D2)✅(`8f8cc248`;fuzz 21.3M 轮零 panic)
- [x] `compile_table` 改返回 `Result` 并**自己调 validate**;三处 expect → 可达具名拒绝;fuzz `table_spec` 双路 21.3M 零 panic 进 CI;docs 保证已写;facadegate 逐字节复刻 F9 事故 spec 断言 Err+零安装

### V3 — declare 生命周期(F8.2)✅(`f57954c4`;双面 + e2e;顺带自抓 python 补丁把 \r\n 展开成真字节、Rust 词法 CRLF 归一的坑)
- [x] `table_ensure` {Created,Unchanged} + `spec_diff` 具名差异 + `table_replace`(坏 spec 在旧表 drop **前**拒);RESP 双面四注册面 + e2e(+UNCHANGED 逐字节/坏 REPLACE 留旧表);docs boot 章归 V8 文档趟

### V4 — VERIFY 单一时间框(D4)✅(`451a552e`;设计中途升级为**双向 walk**)
- [x] 报表全字段每次现算,且发现 4.0 的 lifetime `coerce_failures` 把 absent 也吞了(F10 的 30152"失败"全是 absence)→ `RowDerivation {Indexed,Absent,CoerceFailed,Oversize}` 单一分类同时驱动写路径与 verify;新增 row→index 方向四计数:`excluded` / `absent` / `rows` / **`missing`**(drift walk 结构上看不见的"忘了写的 writer"类,F13/F14 的可见化);两面镜像 22 元素 labeled row(新标签追加在尾,4.0 按标签读的消费者不破),oracle 逐字节;facadegate 每 cause 种一行断言各归各名;lifetime 留 seg stats
- [x] docs:VERIFY 章改写为双向 + `entries = rows − excluded − absent − coerce_failures` 对账;duplicates ≠ 0 ⇒ 非全序 ⇒ 分页跳/重;tie-break 用**有界**列(裸 Message-ID 反例)

### V5 — tiering 收敛:idle 必须近零(D3)✅(`94c23162`;lx64 实测 1.6× vs mailrs 的 300-500×)
- [x] **stats 不再走**:四面 generation cache(embedded ShardSegs/ShardViews + server ShardIndexes/ShardViews,每个 mutation 咽喉点置 dirty)+ **全部 walking stats() 增量化**:kevy-text(postings/tokens/docs/Many-slots + positions/fields 走新 `Channel` + docvalues heap 和)、kevy-vector(links_total/tombstones)、kevy-index agg(distinct/gkey/rowkey);走查器留作 #[cfg(test)] 参照,四个 drift-invariant 测试逐步核
- [x] **采样退避**:tick 零迁移指数跳(顶 64 tick ≈6.4s),任何降温重置;写路径永远立即采样(既有测试锁定),`effective_target==0` 同路收敛;3 个新 kevy-store 单测
- [x] **gate**:tiergate 新 L15 行,lx64 实测 PASS(idle 30s off=7 / on=11 ticks)
- [x] docs/tiering.md:索引地板写在旋钮前面 + idle 收敛契约成文
- [x] **顺带真 bug**:server 面 FLUSHALL 不 reset segments/views,IDX.QUERY 继续吐已删 key(embedded 面会 reset,两面分歧 = D1 类洞)→ 新 `Commands::on_flush` 钩子双路径(client + replica-apply)+ 回归 e2e

### V6 — 运行状态可读(F16.2 + smix 反馈)✅(smix 项 `fe63f9e4`;server 双 gauge `ae2f6552`)
- [x] **smix 项已落**(`fe63f9e4`,第二份 dogfood 输入 `/tmp/kevy-feedback-2026-07-26.md`):`Aof::format()` 公开 + `Store::downgradeable_to_v3() -> Option<bool>`;e2e 伪造 3.x 文件走完 开窗→追加保持→rewrite 关窗 全程
- [x] `# Memory` 加 `process_rss_bytes`(kevy-sys OS 边界:/proc status VmRSS + mach task_info 手写绑定)+ docs/tuning.md 容器按 RSS 定容;`# Persistence` 加 `aof_format`(off/v1/v2,新 defaulted `on_aof_format` 钩子;e2e 断言真 AOF server 报 v2)

### V7 — 错误互操作 + UPGRADING 纠偏(F1/F2/F4)✅(`612041d8`)
- [x] `From<KevyError> for io::Error`:类型单点在 kevy-store,一个 impl 三面全覆盖(kind 映射 + source 保留 + Io 变体直通不双包);**这是对 4.0 "deliberately no back-edge" 设计决定的有据推翻**(280 个 io::Error::other 就是它错了的实证),UPGRADING 原段落改写为反转记录;单测钉死每个映射 + downcast;facadegate 消费者位 `?` 断言 + kevy-client-async 进 gate
- [x] UPGRADING:kevy-client 2.0.0 行核对(已正确,F1 早已解);"迁移实际由什么构成"段 + `--message-format=json` worklist 技法;`with_auto_aof_rewrite_disabled()`(F4,一次清三个自动 rewrite 触发旋钮;config.rs 超限顺带拆 config_tier_builders.rs)+ canary 段接 观测面(downgradeable_to_v3 / INFO aof_format)

### V8 — 迁移专章(D5)✅(`b85821ca`)
- [x] `docs/table-migration.md`:八课按需要顺序全落 + 开篇可验证性论证 + 实测三类漂移(89% never-written / 76% never-removed / 序漂);tables.md 加 boot-pattern 章(ENSURE/REPLACE 语义,V3 归档的文档趟)+ 显眼指路

**明确不在本 arc(记录非静默 defer)**:F13 行新鲜度信号(真特性,独立 RFC);F16 可下沉索引层(= v5 试验 T5,已在那边 roadmap)。

### SHIP ✅ v4.1.0(2026-07-28,tag `v4.1.0` @ `e4b819bf`)
- [x] CHANGELOG + workspace 4.1.0 / client 2.1.0(88+2 pin)+ 双 lockfile + release 预跑
- [x] 文档三语 delta(tables/tiering/tuning/UPGRADING)+ table-migration 三语;顺带修掉 en/ja/zh 嵌入示例仍在教 `use kevy_index::TableSpec` 的 F7 反模式残留
- [x] 站点:700+ 页 4.1、table-migration 三语上站、TABLE.ENSURE/REPLACE 命令页(190 verbs ×3)、llms 快照、4.1 wasm;rsync t01 并线上 200 验证
- [x] CI 五连修后真绿:oracle build-if-absent 陈旧缓存 binary(改永远构建)/ wasm dead-code(cache 字段 cfg + mark_stats_dirty)/ killgate(pgcompare 无守卫 pkill,07-26 起的遗留红)/ locgate 四个 fn>50 拆分 / commentgate 46 处出处注释清零 / **uring EINTR 真 bug**(availgate SIGSTOP clamp 暴露:io_uring_enter EINTR 未重试 → shard 退出;全树唯一缺口,已补重试)
- [x] release.yml success:crates.io 34 crates(kevy/kevy-embedded 4.1.0、client/async 2.1.0 已线上验证)+ npm @goliapkg/kevy 4.1.0 + GH release(6 assets)
- [x] 装后 smoke:crates.io 拉 4.1.0,消费者位 facade import + `?` io 互操作 + ensure + fresh verify 一次通过

---

# v5 试验 arc — 中小企业 datasolution 的**一种尝试**(2026-07-26)

> **这不是 v5,是 v5 的一次尝试**(用户拍板 2026-07-26)。
> 下面每一条都是**待验证的假设**,不是已定的架构。**从原理层就可以调整** ——
> 模型前提、判据、乃至"最佳实践"的既有设计,都可以按试验结果改。
> **不设版本号目标**:它成为 v5,是因为试验结果撑得起,不是因为清单跑完了。

**愿景(拍板)**:kevy 做**中小型企业(SME)的 datasolution**,在中小型 RDS 业务上全面超越 PG。
**"中小型"指企业规模,不是数据规模** —— SME 可以有几千万行、几十 GB;稀缺的是运维人力、
专家与硬件预算。愿景是拍过板的;**下面所有通往它的路线都是可推翻的。**

**研究总案(2026-07-31,主轴拍板:滑动窗口热区的 KV → 虚拟 SQL support)**:
`.claude/plans/2026-07-31-v5-research-master-plan.md` —— 七个研究单元 R1-R7 与合成路径;
各单元开工前仍需自己的 RFC。开工顺序建议:R2a+R2d 窗口模型设计轮 → R1 residual 设计轮 →
R4a 覆盖度清单(可并行)。

**主设计**:`.claude/plans/2026-07-26-v5-arc-design-input.md`(SME 判据 S1-S5 / 模型固有 I1-I5 /
四项约束拍板 / 核心张力)· `.claude/rfcs/2026-07-26-v5-kevy-alloc.md` ·
`.claude/rfcs/2026-07-26-v5-kevy-compress.md`。
体检单(**不是设计输入**):`bench/PGCOMPARE-2026-07-26.md` + `bench/PERF-DECOMP-2026-07-26-idx-fanout.md`。

## v5 arc 专有铁律(在总则之上追加)

**① 不是改善,是设计**(用户 2026-07-26)。验收线写**结构性陈述** ——
"这个设计之后还剩哪几项开销、各自怎么 scale",数字是推论不是目标。
**禁止**把"比 PG / 比 glibc 好 X%"当验收线。对比在打磨之后做,不作设计输入。

**② 站在巨人肩上,但自研**(用户 2026-07-26)。零依赖铁律保持;RFC 必须**具名列参照系
并写清各自贡献什么**(不是"参考了业界做法")。

**③ 怕耦合就分层,不要纠结**(用户 2026-07-26)。ceiling-first 与"石头不许有业务耦合"
**不是二选一** —— cement/steel 包 stone 即可:石头只认字节/纯结构,业务知识放在包着它的那层。
**禁止**因为"会耦合"而砍掉能力;正确动作是把能力放到正确的层。
(已用例:kevy-compress 的字典种子是**参数**不是依赖 —— 石头不知道"声明"是什么,
由包着它的那层把声明渲染成字节传进去。)

**④ KV / pubsub 不得退化**(C4 拍板)。分配器**没有"关掉"这一说**,所以验收是
"**打开时**不退化",不是"关掉时字节不变"。

**⑤ 前提可推翻,试验结果高于清单**(用户 2026-07-26:"还不能算是 v5,只是 v5 的一种尝试,
从原理开始就可以调整,最佳实践的各种设计也可以做调整")。三条操作含义:

- **前提被证伪 → 改前提,不许绕过去。** 例:若 T2 实测显示碎片的主项不在分配器
  (像 v1.29 的 "memcpy 是 tax 不是 bottleneck"),那么"自研分配器"这条路线本身作废,
  回去重做分解 —— **不许**改成"再 polish 一版分配器"。
- **负结果是结果,要留档。** 试验被推翻时写 finding doc(体裁同 `bench/PERF-FINDING-*.md`),
  说清哪条前提死了、被什么数据杀死。**清单上打叉不是失败,是产出。**
- **巨人的设计也可以改。** 铁律② 要求具名参照,但**不要求复刻**。若我们的模型
  (每核独占、sized dealloc、语料而非 datum)指向与 mimalloc / LZ4 不同的结构,
  就按我们的模型走,并在 RFC 里写清**为什么这里与参照系分岔**。

**这条 arc 什么时候才配叫 v5**:两块石头的结构性验收(M3 / K4)真成立、
KV+pubsub 打开时不退化、且 SME 口径的产品陈述(一台 32 GB 机器能装多少业务)拿得出数。
**达不到就不叫 v5**,把学到的东西留下,重新设计。

**v4 遗留**(不阻塞 v5 起步,但不许静默消失):t5 的 lx64 arena 复测 → README 基准表解冻;
t6 剩余渠道(brew tap / apt on t01 / npm 平台分包 / NuGet push / kevy-go 剥离)。

## 线性 checklist(v5,从上往下,不跳序)

> **⚠️ 陈旧警告(2026-08-06 核实)——T0 / T1 / T2 已经做过了,不在这条分支上。**
> 它们活在 **`r1-locality`**(本地 + `origin/r1-locality`,**32 commit 领先
> develop、111 落后**):`bench/allocgate.sh` / `allocgate-mem.sh` /
> `compressgate.sh` 都在(= T0),`crates/kevy-alloc/src/` 有 12+ 个文件
> (class / global / heap / heap_claims / heap_foreign / heap_hot / large /
> os / outbound / pagemap …)(= T1),并跑到了 **v8 收口**
> (`17f85688 finding(v5): the v8 closing ledger — seven of twelve, and the
> residual has a name`;十二角过七、M3 1.98× vs 2.40×、residual 已具名 =
> 集合写小分配的元数据远线,**天真修法已知会破坏 M3,设计轮待拍板**)。
>
> **这条分支(`fix/idx-drift-on-multikey-writes`)上留下的痕迹只有
> `crates/kevy-alloc/fuzz/`** —— 3049 个语料文件 + 2 个 OOM artifact 被
> 跟踪着,而 `src/` 与 `Cargo.toml` 从未进过这条线。所以本地看起来"crate 存在
> 但是空的",这是**分叉的假象,不是半成品**。
>
> **照着下面的未打勾去开工,会把一条已经存在的 train 重做一遍**——本轮起手
> 就差一步走进去,是先核前提拦住的。下面的框保持原样(它们记录的是设计,
> 不是进度);**真进度以 `r1-locality` 为准,merge 归属主**。

> **同日把九条 train 逐条对了一遍账 —— 下面的框有四条是"做完了没打勾"。**
> 每条给的是**能核的证据**,不是判断:
>
> | T | 框显示 | 实际 | 证据 |
> |---|---|---|---|
> | T0 门先行 | 未打勾 | **已做**(r1-locality) | `bench/allocgate.sh` / `allocgate-mem.sh` / `compressgate.sh` |
> | T1 alloc 石头 | 未打勾 | **已做**(r1-locality) | `crates/kevy-alloc/src/` 12+ 文件;v8 收口十二角过七 |
> | T2 alloc 接线 | 未打勾 | **已做**(r1-locality) | `perf(v5-T2)` 系列提交;M3 1.98× vs 2.40× |
> | T3 compress 石头 | 未打勾 | **真没开工** | 全仓只有两条 RFC 提交,`crates/kevy-compress` 任何分支都不存在 |
> | T4 compress 接线 | 未打勾 | **真没开工** | 同上 |
> | T5 索引冷热窗口 | 未打勾 | **已发布** | `crates/kevy-window` + `crates/kevy-seg` + `kevy-index/src/segcold.rs`;`WINDOW col SPAN n BUCKET n` 语法在册;tiergate 六条窗口线;三语文档(zh/ja 于 2026-08-06 补齐) |
> | T6 自动声明闭环 | 未打勾 | **已发布** | `AUTODECLARE` 横跨 5 个源文件 + `crates/kevy/tests/idx_advise_e2e.rs` |
> | T7 索引即键(I2) | 未打勾 | **RFC 已出待拍** | `.claude/rfcs/2026-08-05-v5-i2-single-hop-index.md`;**批准前零实现代码** |
> | T8 部署配方 | ✅ | **已做**(2026-08-06) | `docs/deploy-behind-a-proxy.md` 三语 + 站点 |
> | T9 试验终账 | 未打勾 | **开着,被 T3/T4 与 merge 挡着** | 判据要 allocgate + compressgate 双绿 |
>
> **⇒ 真正开着的只有 compress 那条 train(T3/T4),外加属主拍板的 T7 与 merge。**
> 之前"九条几乎都没动"的观感是清单造成的,不是事实。

> 每 train:开工第一动 = feature 分支;标【RFC】的 RFC 批准前零实现代码;finish 前五轴收口全绿。

### T0 — 门先行(先红)✅ 完(2026-07-26,`feature/v5-memory`)
- [x] `bench/allocgate.sh`:M1-M8 逐条落成(9 行,标 train 归属);**RED exit 1 如设计**
- [x] `bench/compressgate.sh`:K1-K7 同上(8 行);**RED exit 1 如设计**
- [x] **记账契约定死** = `bench/V5-ACCOUNTING-CONTRACT.md`(两 crate 要导出的字段名 + 恒等式 + 拒绝条款)。**恒等式不设容差** —— 未被解释的字节正是 glibc 那 2.24× 藏身之处;"加一项去凑平"明令禁止
  - **诚实降级**:compress RFC §6.1 的四项里 **incompressible residual 与 match-finder miss 靠记账不可分**(要知道漏了多少冗余,就得先知道有多少 —— 那就是压缩问题本身)。契约不假装:合成一项 `cmp_payload_bytes`,拆分改为**带外**用 spg lzss 的**暴力穷举 matcher 当 oracle** 实测(T3 出 finding doc),不进每轮 gate
- [x] **M8 当场变成真断言(T0 唯一绿灯)**:反例验证过(给 `kevy-hash` 加一个 `unsafe fn` → FAIL,还原 → PASS)。**顺带抓到 RFC 一句不实陈述** —— 我写"unsafe 只在 kevy-sys/kevy-uring/kevy-madvise 少数几个 crate",实录是 **14 个**(FFI 门 / wasm ABI / raw-entry map / uring reactor 本来就该有)。RFC 已更正;M8 改为对 `bench/.unsafe-crates-baseline` 的 **ratchet**(不看数量看不许悄悄增长,`kevy-alloc` 预批)
- [x] **计划外偏离(已论证)**:原写"perfgate 新增 allocator-ON 对照线",实施时改放 allocgate。理由 = 两者问的不是同一个问题:**perfgate 是对历史基线的 ratchet;分配器问的是同源两构建、同盒、交错的 A/B**。perfgate 待 T2 末分配器默认 ON 后,其既有线自然就在测它,**无需新增 metric**

### T1 — `kevy-alloc` 石头(不接线)✅ 完(2026-07-26,`feature/v5-memory`)
- [x] **几何**:segment 4 MiB(按 4 MiB 对齐 map)× 64 span × 64 KiB,span 0 放头。**指针掩码即查表** —— `ptr & !(4MiB-1)` 得 segment、偏移得 span、span meta 得 class,于是**每块零 header**(mimalloc segment/page + Go mheap arena 索引)
- [x] **size class 63 档**(每八度 8 等分,最坏相对舍入 11.1%)+ 空槽内联 freelist + 按 class 的 span 归属
- [x] **外分片 free = push-only + swap-drain**(mimalloc thread-free 式)。**ABA 不是被接受而是被消除**:torajs `central.rs` 记录并接受了 Treiber pop 的 ABA(理由是它单线程),kevy 真有 N 核并发 free,**继承那条注释就是继承一个 bug**。改成只有属主移除、且一次 `swap` 取走整条链 —— 消费侧没有 CAS,ABA 无窗口可开
- [x] `PER_CLASS_CAP = 64` span/类 + 耗尽答 `None`(torajs `c2970b6d` 的学费)· jemalloc decay 式 `EMPTY_SPAN_HYSTERESIS`
- [x] **记账七项**(T0 契约 §1)+ `Stats::balanced()` 恒等式无容差;`snapshot()` 走 segment 计算,热路径只维护 live/rounding 两个计数器
- [x] 验证:**21 单测全绿**(darwin;Linux/wasm/thumbv7em 交叉 check 净)· **fuzz `heap_churn` 431,413 runs 零 crash**(已进 fuzz.yml)· clippy/locgate 绿 · release.yml 发布链已登记
- [x] **bench 入 STONE-BENCH**(与 system allocator 并排):alloc+free 400B **5ns vs 18ns**;churn 4096×400B **3.8 vs 19.5 ns/op**;**含归还页 29.3 ns/op —— 该行没有对手列,因为那是 glibc 出多少钱都做不到的操作**
- [x] **allocgate 四行翻绿**:M3-identity / M4-reclaim / M6-class-cap / M8-unsafe-ratchet(其余五行归 T2)
- **三处与参照系分岔(铁律②要求写清)**:① **不放 TLAB** —— tcmalloc/mimalloc 的线程缓存是为了挡在共享堆前面,kevy 每核一分片,堆本身就是线程本地的,快路径已无原子,再加一层是缓存前面套缓存;② **span 统一 64 KiB 而非按 class 变长** —— 初稿按 class 变长以压 slack,被两点推翻:掩码需要统一几何(变长要在 free 路径上加查找结构),且 span 尾部是 bump 之上的 **virgin**(已映射但从未触碰 = 不常驻),担心本身错位;③ **OS 边界自建而非复用 `kevy-madvise`** —— 后者 Linux-only 且契约就是大页建议,而分配器要 macOS 也能跑、且要能**归还**页
- **两处自测抓到的真错**:① `MIN_ALIGN=16` 是假的 —— 步长 8 的类(24/40/…)只有 8 对齐,已改为 class 选择满足对齐(`index_of(size, align)`);② "8 字节步长不额外花钱"是假的 —— 16→24 相对浪费 29%,已改成两段式声明(≥128 相对界 <12.5%,以下**绝对界 ≤7B**,那是 8 字节粒度的地板不是选择)
- **一处 bench 自欺已修**:首版把 `reclaim()` 计进 churn 行,读出来是"慢 1.5×" —— 实为**拿我们的归还去比对手的什么都不做**。已拆成独立行,并注明该行没有对手列

### T2 — `kevy-alloc` 接线 + M1/M2/M3 实测(lx64)—— **进行中,已出一条推翻前提的 finding**
- [x] `#[global_allocator]` 挂 `kevy` 二进制(feature `kevy-alloc`,默认 OFF)+ `KevyAlloc` 导出给嵌入方;TLS 堆用 `ManuallyDrop`(线程退出**泄漏地址空间而非 unmap 活内存** —— 值跨分片共享,segment 可能比分配它的线程活得久)
- [x] **把分配器装进测试二进制,让标准库当调用方** —— 当场抓到两个单线程测试够不到的缺陷:①跨线程 free 扣了**释放方**的计数器(那些字节记在属主堆上)→ 改为释放方把请求尺寸写进空槽 + segment 原子记在途量,属主 drain 时结账;②大块路径同病但无处结账(直接映射无 segment 无属主)→ 计数器改进程级,`Heap::snapshot` 不含,免得按分片求和时重复计
- [x] **`PER_CLASS_CAP` 继承值错了三个数量级**:64 span = 4 MiB/类,合法程序当场爆 "memory allocation of 6152 bytes failed"(机器还有几十 GB)。改为真正的失控护栏 + `with_class_cap` 保住可测性
- [x] M1 委托 perfgate(**不重写测量循环** —— 它已有交错 + 每实例翻序,抵消盒况漂移);perfgate `refuse`(缺工具/盒子脏)报 PENDING 不报 FAIL,但**仍不许 PASS**
- [x] M5 绿(5 条,KevyAlloc 作进程分配器);M4 在 Linux 上**两条都绿**(内核 RSS 真回落)
- [ ] **M2 在 lx64 FAIL:0.84,地板 0.92 —— 六轮两个分布完全不重叠**。`perf stat` 说我们**指令少 6%、缺页 1415→61,却多花 12% 周期**(IPC 1.53→1.29,cache-references +17%)。根因 = **无 header 不是白拿的:它把元数据搬离了数据**。glibc 的 chunk header 和 payload 同 cache line;我们的 span meta 在 64KiB–4MiB 外的 segment 头,每次 alloc/free 多碰一条线。**推翻 RFC 两处**:①"零 header 是纯赚"是**取舍**不是赢;②"不要线程缓存"的论证不完整 —— 线程缓存**也是局部性装置**,不只是避锁。finding:`bench/PERF-FINDING-2026-07-26-header-free-costs-a-cache-line.md`
- [ ] **一次失败的 round + 一次被跳过的门(自记)**:先实现了 in-place `realloc`(0.852→0.843,没动针)。profile 里 libc realloc 只占 **2.32%**,而方法论的 **Pre-Phase-B 门要求攻击目标 ≥ 双位数 pp** —— 门会拒掉这次改动,而我没跑门。realloc 本身值得留(没有它每次扩容都拷贝),但它不是这个问题的答案
- [ ] 下一步(未决,需你拍):把当前 span 的 free-list 头缓存进 heap(mimalloc 的位置)/ 小类把元数据放回数据旁(等于认输)/ 接受这条形状上的损失换内存(**但 M3 内存数还没测,没人能称这笔账**)
- [ ] M1 仍未测:perfgate 拒绝在盒子有其它负载时跑
- [x] **M3 内存那半已测,结果为负(2026-07-27,用户指令"先测 M3 内存那半,拿到数再决定")**。lx64,2M×400B/512MB 预算,同 commit 两二进制:**OFF 341.2MB 逻辑 / 818.1MB 常驻 = 2.40×;ON(reclaim 未接)2.39×;ON(reclaim 接上 shard tick)790.5MB = 2.32×**。对上 pubsub 的 0.84 → **拿 16% 吞吐换 3% 内存**。
  - **中间那行的意义**:`Heap::reclaim` 写了、测了、导出了,**没人调用** —— 分配器没有自己的 tick。空 span 既留映射又留常驻,等于把 glibc 的失败模式用"漏接"复现一遍。接上 tick 后机制确实工作,只是**找不到多少可还的**
  - **真正的前提之死**:**span 只有全空才能还,而 416B 类的 64KiB span 有 157 个槽** —— 157 个值全死才还得回一页;glibc 的单位是 4KiB 页,约 10 个值。**我们的回收粒度比要打败的对象粗约 16 倍。** mmap-backed 是必要不充分:决定回收的是**能交还的粒度**,而 slab 分配器的天然粒度就是 slab
  - 真答案是**在 span 内按页归还**(jemalloc/mimalloc 都做),那是另一种结构不是旋钮:span meta 要按页记占用,free 路径要察觉某页最后一个槽走了。**不是答案的两条**(记下来免得被当答案试):更小的 span(把问题换成另一项)/ 更频繁地 reclaim(扫描已每 tick 跑,限制在于可还的量)
  - finding:`bench/PERF-FINDING-2026-07-27-m3-the-memory-half-does-not-pay.md`。**决定权在你**:改回收结构 / 改目标(承认赢面在别处:微基准 3-4×、小值零 header 占用)/ 停
- [x] **v2 结构落地(2026-07-27,用户拍板"完整这个设计,ceiling-first + clean")**:free list 换 **segment 头内位图**(数据页零元数据 → **页粒度归还**,回收单位 64KiB→4KiB)+ **最低位优先分配 = 主动致密化**(churn 把空闲空间往上挤成整页,无拷贝的类压缩,LIFO free list 表达不了)+ 记账新增 `returned` 项(映射未常驻)。31 单测(Linux 含两条内核 RSS 直证)+ 218k/99k fuzz + clippy/locgate 绿
- [x] **M3 内存半:2.40× → 1.98×,双向复测稳定**(818→676MB @ 341MB 逻辑;v1 只 -3%,v2 **-17%**)。非天花板:剩 335MB 未归因,待七项导出
- [x] **M2 吞吐半:三个机制假设全被测量淘汰**(in-place realloc 0.852→0.843 / 位图重构元数据 0.826 / 热缓存消除 churn 路径全部 segment 触碰 0.844)。**缺口按 payload 分形**:16B 0.92 / 64B 0.84 / 4096B ~1.0;**perf 挂上时缩到噪声甚至反转** —— 是时序/布局效应不是热符号,三次符号级修复无效与此自洽。剩余假设空间 = 布局形(着色/预取/TLB),**需要专门的分解轮,不许第四次盲修**。finding:`PERF-FINDING-2026-07-27-v2-memory-delivered-throughput-gap-unexplained.md`
- [ ] M3 七项在 envelope 尺度的分解导出(INFO `# Allocator` 段)—— 剩余 335MB 的归因需要它
- [x] **外分片路径重设计轮(2026-07-27,用户拍板;两轮结构 + 一次判别)**:
  - **v3 批量回家**:free 快路径 = 本地环两次 store;flush 按 segment 分组、槽内穿链、**整批一记 CAS 拼接 + 两记 fetch_add**(跨核流量摊薄 ~百倍);drain 端格式不变;所有权照旧精确。**A/B 裁决:纹丝不动**(compat_get −40.1% vs −38.5%)—— **机制候选①③(原子流量/drain 走查)死**
  - **判别实验**:同一 compat 拓扑 `--threads 1`(跨分片不可能存在)→ **1.006 / 1.023 全净** ⇒ 触发器确是跨分片形状,但成本不是流量,**是内存冷热**
  - **v4 吸收复用**:稳态跨分片流里 B 的 free 全外来、B 的分配全死在别处 ⇒ home-routing 让热缓存**结构性饿死**,每次分配摸冷 span(唯一能事后解释 v2 热缓存无效的机制,计数器签名全吻合)。tcache 式吸收复用,三方记账问题用"**segment 属主 = 永久会计人**"解:吸收记 (−r,−(s−r))、复用记精确逆,负和恒等于外方缓存停驻字节,第三分片只需地址;每 custody 变更两记 relaxed fetch_add(**执照 = v3 证明原子不是成本**)。custody-cycle 三方逐步恒等式测试;被吸收槽故意不随 tick 回家(暖供给)。31+5 测试 + 110k fuzz 绿
- [x] **v4 裁决:跨分片纹丝不动 + 打崩同分片 SET(−50.3%)→ REVERT**。custody 洞:A-拥有/B-复用/**A-亲自释放**走 local 分支双重记账(测试只走了 C-释放)。第五个死机制
- [x] **布局 probe:零**(全函数 64B 对齐归一,比值同带)—— 第六个;副产品 = 证明缺口在 compat harness 上**经得起剖析器**
- [x] **差分剖析终于点名(2026-07-27)**:`osq_lock` 15.9% + rwsem 自旋 + `__x64_sys_munmap` 27.6% + `vm_mmap_pgoff` 12% = **40% 服务器时间在 mmap/munmap 两个 syscall,8 分片串行在进程 `mmap_lock`**。凶手是大块路径那行注释:"no pooling; large allocs are assumed infrequent"。全部异象归位(threads=1 净/cluster 净 compat 输/五轮小路径零/微基准快/对齐零/IPC 低)
- [x] **v5 类表扩 32 KiB**:compat −40 → −9.1
- [x] **v6 partial rings**:无效于吞吐(留作 v8 的正确结构);**v7 删热缓存:M3 从 2.38× 复原 1.98×** —— LIFO 复用破坏致密化,137MB 静默蒸发了四轮(教训:**为轴 A 的改动若可能触碰轴 B 的机制,必须复测轴 B**)
- [x] **v8 进程级映射池**:syscall 计数抓到还剩 105k mmap/6s(36–300KB 增长阶梯);每堆池**零效果**(缓冲跨分片生死,池在错侧)→ 进程级 + 自旋锁(锁下收集、锁外 munmap)→ **mmap −97%**。fuzzer 几分钟抓到 `Heap::drop` 忘排池的真泄漏
- [x] **v8 终裁:12 角 7 过**(compat_get −2.8 / legacy_get **+2.1** / zalg **+7.5 反超**;compat_set 差 0.035% = 噪声距离);**M3 保持 1.98×**。residual = 4 条集合写线 −10.6~−18.6,**已剖面孤立**:hset 上分配器 17.3% vs glibc 10.1% 自身时间 = 最初的元数据远线故事;天真修法(热缓存)已知破坏 M3。finding:`PERF-FINDING-2026-07-27-v8-closing-ledger.md`
- [ ] **residual 设计轮(待拍板)**:既回收热槽又保持位置感知(候选:仅持最低 span 槽的有界缓存 / per-word 位批发 / 或接受集合写 −10~−19% 换 −17% 内存 —— SME 取舍归属主)
- [ ] M2 pubsub 0.858(同 residual 类;池不覆盖它)
- [x] **M1 已测(2026-07-27,盒 98% idle 时 perfgate 全套 A/B 真跑):判别子干净** —— **同分片 KV ±1% 全净**(pinned_cluster get/set +0.3%/-0.9%,conn=属主),**跨分片 KV -18~-39%**(compat -38.5%/-39.2%,legacy 七线 -17.6~-28.4%,zalg -24.6%)。**快路径无罪,外分片 free 路径定罪**;pubsub 是 --threads 1(无外分片 free),两个回归就此分开。机制候选(待分解轮判):①每次外分片 free 一记跨核 CAS(七个分片捶同一 segment 原子,RFO ping-pong;compat_get 基线 ~52ns/op,一记争用 CAS 足以解释)②glibc tcache 把外来 chunk 推进**释放线程自己的**缓存并**本地复用**——我们的设计每个外来槽都要绕道回家;③`drain_foreign` O(全 segment)。finding:`PERF-FINDING-2026-07-27-m1-foreign-frees-are-the-kv-killer.md`
- **三轴账目(T2 完整测量后)**:内存 **-17%** / 同分片 KV **±1%** / 跨分片 KV **-18~-39%(C4 直接不过)** / pubsub 小包 -8~-16%。**现状不可默认 ON**;外分片路径需要重设计轮(tcache 式本地复用 + 相应的所有权记账)才能重新称账
- [ ] 大页作**旋钮**对着 M1/M3 实测(`MADV_HUGEPAGE`,非 `MAP_HUGETLB`;span 仍是细粒度回收单位)
- [ ] 全绿后才谈默认 ON;M7 既有门全绿(crashgate/availgate/tiergate/tablegate/textgate/oracle)

### T3 — `kevy-compress` 石头(不接线)【RFC 已批】
- [ ] frame 格式(`[tag][orig_len][payload]`)+ 快速 LZ 级:哈希表单探 match finder / 64 KiB 窗口 / token nibble + 续字节变长 / 8 字节 wildcopy 解码
- [ ] 字典:采样训练 + **可选调用方种子语料 `&[u8]`(参数不是依赖,铁律③)**
- [ ] **K2 永不膨胀**(incompressible 早退,Snappy 式)· **K3** round-trip fuzz + 截断/损坏帧必须拒收而非误解码
- [ ] **K4 结构性断言**:同一 segment 内 N 个相同的 400B 值 → O(字典) + N×小;per-datum 基线**证明性地过不了**
- [ ] 四项记账导出(T0 契约)+ 对 spg lzss 的 bench 对照(编码/解码/比率三轴)

### T4 — `kevy-compress` 接线(降温 + 压实 + 冷读)
- [ ] 降温批路径编码 → `Vlog::append` 收已编码 body(**磁盘 framing 不动**,tag 在 body 内,CRC 覆盖面不变)
- [ ] `compact_below` 解码-重编码(§3 两级结构现在就建,v1 两级同参)
- [ ] 冷读 `read_at` → 解码;**K1 冷读 p99 仍在 B2 预算内**(145µs hash / 105µs scalar)
- [ ] 字典生命周期:随文件生灭(可弃性继承 AOF 唯一真相)+ **轮转时用前一个文件的字典播种**(冷启动按构造消失)+ pin 语义不变
- [ ] **K6 静态断言:`SET` 路径上不存在任何 encode 调用**(按检查证明,不只靠跑分)· K5 vlog 放大对 1.27× 改善且压实仍终止 · K7 tier_persistence B10/B11 不变

### T5 — 索引层冷热窗口【RFC】
- [ ] 前提陈述:索引 100% 常驻 RAM 是**设计决定不是物理必然**(capacity arc 原文);I4 推广到访问路径
- [ ] RFC:哪些索引结构可下沉、下沉后查询延迟的结构性陈述、与 IDX.CREATE floor 拒绝的关系
- [ ] 五轴 + 既有 tiergate/idxgate 不回归

### T6 — 自动声明闭环【RFC,本 arc 模型价值最高】
- [ ] 核心张力(设计输入 §3):Law 3 让"不必调优"但"要求前瞻";**引擎观察真实查询、自动完成声明**
- [ ] 与 planner 的分界写死在 RFC:**决定在声明期(一次),不在查询期(每次)** —— 延迟仍无执行计划意外,derived-by-construction 不破
- [ ] 分层(铁律③):观察层是 steel,包着 kevy-index 石头与声明层;石头不认识"查询历史"
- [ ] `kevy-sql` 已做了一半(声明期编译 + 缺哪个索引的具名报错)—— 差自动闭环
- [ ] 拒绝面不许缩水:ad-hoc SQL / PG wire 仿真仍拒(C2 的松动是**自主设计**,不是把 Law 3 作废)

### T7 — 索引即键(消 I2 特例)【RFC】
> **RFC 已出,待拍板**:`.claude/rfcs/2026-08-05-v5-i2-single-hop-index.md`
> (设计轮:四方案含"不做" + 四个拍板题 + 四切片)。**批准前零实现代码。**
> R3 的实测已经把这条 train 的前提坐实:SME 4-8 核上 kevy 两轴本来
> 就赢 PG(idx p99 72-80 vs 164、page 111 vs 147),16 核那档的 p99
> 悬崖是部署形状(`--threads ≤ cores − 2`),A3 落地后 page p50 与
> PG18 打平 —— **所以下面这句"理由是模型一致性不是性能短板"是对的**,
> RFC §三b 按它改了口径。真难点在 s1:按 hash 分区的全局索引只能答
> 等值,而输给 PG 的两条里 page 是范围 ⇒ 必须按值**序**分区。
- [ ] 模型最干净的一条:索引项本身就是键、按同一条路由规则分片 —— **消特例而非加特例**
- [ ] **诚实前提**:按 SME 的 4-8 核重读,索引查询 kevy 本来就赢 1.9-2.8×(扇出是 16 核放大的问题)⇒ 本 train 的理由是**模型内在一致性**,不是补性能短板
- [ ] 写侧跨分片维护复用既有 xshard/escrow/outbox;单连接延迟与并发吞吐两头都要守

### T8 — 部署配方(C3 的答案,小,可并行)✅(2026-08-06)
- [x] `docs/deploy-behind-a-proxy.md`:反代 + TLS 终端 + 只暴露必要端口,三语 + 站点。
      **三条实测结论进了正文,每条都是会被照抄然后失败的那种**:① **HTTP 反代载不动
      RESP**,包括 stock Caddy(核心无 layer-4 模块,实测 2.11.4)—— 而愿景里那句
      "caddy 包裹"对 HTTP 成立、对 RESP 不成立 ② **`kevy-cli` 连不上被 TLS 终止的
      kevy**(它对 `rediss://` 回 `Unsupported`)③ **单机集群模式撑不过代理**
      (广播的是 bind 地址,`0.0.0.0` 回落 `127.0.0.1`,且没有 announce 旋钮)。
      实测的是**形状**:TLS 1.3 终止器 → 未经修改的 kevy,回环端口与 unix socket
      两条都 RESP 完整往返;三段产品配置按标准写法给出并**注明没在这里跑过**。
- [x] **计划外:写这一章时抓到第九个同形状缺陷** —— `KEVY_UNIX_SOCKET` 在
      kqueue / epoll 两条 reactor 上**只绑不服务**(socket 文件在、connect 落进
      backlog、永远等不到 accept,且不报错)。选 listener 的是一个 `cluster: bool`,
      而 unix listener 是在这个 bool 之后才来的。已改枚举 + poll 循环注册并 drain,
      补首个 UDS 回归测试(**等回复而不是等文件**,并 `KEVY_IO_URING=0` 强制走那条
      本来没人走的路);暂存修复实测两测皆红、恢复后 0.9 秒绿。
- [ ] 引擎侧 AUTH/TLS 仍 OUT(`feedback-kevy-auth-tls-never` 不变;愿景变了也不解锁)

### T9 — 试验终账(**判定这次尝试算不算 v5**)
- [ ] allocgate + compressgate 全绿 + capacity envelope 复跑(alloc/compress ON)
- [ ] **然后**才做对比(纪律①):PG 复测同一套 `bench/pgcompare.sh`,数字如实入账,输的轴列明
- [ ] SME 口径的产品陈述:一台 32 GB 机器能装多少业务(由内存比值决定)
- [ ] **判定**(纪律⑤):撑得起 → 走五轴终审 + CHANGELOG + tag/publish(用户拍板版本号);
      撑不起 → 写 finding doc 说清哪条前提死了,**不发版**,回设计轮重来

---

## 当前 arc — v5 工业版(2026-08-07 属主转轨定向)

> **T9 的判定已由属主给出:研究部分足以支撑,v5 转入工业版本轨道。**
> kevy 本身的提升要做,RDS 支持作为 v5 的**主要附加模块**也要做;
> 测试标准、产品能力标准、用户面产品形态三套标准重订。
> **章程(三套标准 + 判定依据)**:`.claude/plans/2026-08-07-v5-industrial-charter.md`
> 拍板件 P1-P4 见章程 §五(不挡 V0-V3 开工)。

### V0 — 合并轮(无前置拍板,可即刻开工)
- [x] `fix/idx-drift-on-multikey-writes`(数据丢失修复 + R4c 迁移工具链 + 边界/文档)merge → develop(2026-08-08,`ac5a3f53`)
- [x] `r1-locality`(kevy-alloc 三刀 + drain 修复 + perfgate-median + kevy-compress 全弧)rebase + merge → develop(`e17b75ac`;rebase 三处冲突手工收,evicted_keys 改落 `info_sections.rs`,tick 拆分复位 50-LOC)
- [ ] 合并态全门禁 + CI 真绿(`gh run watch --exit-status`);**不发版**
      —— 本地+盒上已全绿(2026-08-08):workspace 233 套件 exit 0 / clippy -D warnings 0(修三处)/ locgate/commentgate/rootgate PASS / crashgate·repligate·idxgate·envelope·compressgate PASS / tiergate 有测量体的行全 PASS(L4 是仪器修复 `42c1a956` 后真 PASS;L1/L3 设计内 PENDING)/ perfgate-median 12 角 median-of-3 全 PASS。**剩 CI = push 归属主**

### V1 —【RFC】标量函数面(RDS 模块商用性的最后一块工程)
- [x] RFC:`.claude/rfcs/2026-08-08-v5-v1-scalar-functions.md`(2026-08-08)——落点被 Law 3 钉死在 sql 面(引擎零改动);**章程 bar「89×80%」实测不可达(天花板 ~50%),重锚 = 拍板点①**;姐妹先例清点(spg eval 家族 + 3236 行纯 Rust ERE)
- [ ] 实现 + funcgate(大部已落,2026-08-08):`kevy-scalar` 石头(36 函数,探针转写语义,三分量 interval)/ kevy-sql `fold_select` + `sql eval` / `sql probe` 分类 runner / **`bench/funcgate.sh` 已立:wrong==0 硬线 + subset-foldable 74% ratchet(356/479)**;语料 89 件已入库。**余项:S3 regexp(拍板点③,推荐 fork spg)/ md5(拍板点④)/ bar 终值(拍板点①)**

### V2 — 迁移演练门 ✅(2026-08-08)
- [x] 真 PG 库端到端:自有 postgres:18 容器 + 52k 行确定性种子;**四堵墙全 finding 化并修**(NOT NULL/DEFAULT 致命拒 → 注记;单表坏类型杀全 plan → lenient dropped 行;pg_dump 方言七构造含 ALTER 携带的 PK 回填;**引擎对 redis-cli --pipe 的裸 CRLF 回幽灵 ERR → 空解析静默吞掉,Redis 语义**)。总账 `bench/FINDING-2026-08-08-migration-drill-four-walls.md`
- [x] `bench/migrationgate.sh`:seed→dump→plan 断言→day-2 apply→COPY→帧→import→行数+抽样对账→VERIFY drift 0→doctor,全 trap 清理;**连跑两遍 PASS(可重复性证毕)**

### V3 — 尾延迟工业化
- [x] 心跳探针机制化 + tailgate ✅(2026-08-10 S4→S5→R2b 全弧:出厂默认双 cell 双 bar PASS——mixed gap 43-48ms/PING p999 sub-10ms、firehose gap 24-78ms;AOF offload 默认化 + 两段式 rewrite 全离场设计;方法论与证据链 = bench/FINDING-2026-08-10-s5*.md + memory RESUME)
      —— **仪器已落地(2026-08-08)**:`reactor_tick_gap_max_us` 引擎 gauge(tick 迟到 = 单迭代停顿上界,10Hz 零热路径成本,双 reactor 接线)+ `examples/tail_probe.rs` 进程内探针 + `bench/tailgate.sh` 两 cell。**基线(诚实 RED)**:mixed p999=100.8ms/gap 6.07s、firehose p999=280-460ms/gap 1.3-2.3s(×3)。**副产物:仪器首日抓出并修掉一个必崩缺陷**——单值 >4GiB 时 AOF 重写单帧 Argv u32 偏移回绕、persist 线程带崩全进程(`f94c3635`,按 Redis AOF_REWRITE_ITEMS_PER_CMD=64 分块 + 256MB 帧字节界)。**余项 = 两类停顿的 Phase A decomp + attack**(mixed 的 6s 与 firehose 的 AOF 追加路径)
- [x] 慢客户端 / 连接风暴防护核查(2026-08-08)—— 三面审计:输出侧 `CLIENT_OUTPUT_HARD_LIMIT` 512MB 每 tick 扫(双 reactor,已有)/ 接入侧 maxclients 按 shard 分摊 + rejected 计数(已有)/ **输入侧无界 = 真缺口**:合法但永不完帧的巨型 multibulk 可把 `conn.input` 撑到 OOM → 补 `CLIENT_INPUT_HARD_LIMIT` 1GB(Redis client-query-buffer-limit 同值)双 reactor 读时执行,`KEVY_DEBUG_INPUT_LIMIT` 可覆盖,e2e 钉死断连+服务器无恙(`a692525c`)

### V4 — alloc 执行轮 ✅(2026-08-09 收卷:enabler 落地 + P1 终判 opt-in)
- [x] hash 小值内联 enabler 合 develop(`2e242e78`):HashData 值槽 →SmallBytes,alloc-ON hset 角税 −5.8→−0.2 清零,+12.7% 流水线 hset,RSS −3.2%,全门禁绿
- [x] 完整 P2 定价(perfgate-median ×3/×5 alloc-ON):10/12 PASS;sadd −9.7 / zadd −13.5 为真距离
- [x] 残税 Phase A 决算(`bench/PERF-DECOMP-2026-08-08-zadd-sadd-alloc-tax-split.md` + 两勘误):sadd/zadd 同源 = 饱和 owner 快路径每调用成本;转发信封与 tree-walk 假设均证伪;B' 不适用、B 数字不够、C 无主导座位
- [x] **P1 终判 = A 案(opt-in)**,fastpath-residue RFC Resolution + charter §六 已记;用户侧何时开 alloc 指南 = docs/alloc.md

### V5 — 产品面
- [ ] 文档四区重构(Core KV / RDS 模块 / 运维 / 迁移指南)三语 + 边界页(14 条 + 函数面进度)
- [ ] 容量计算器(公式 + 值大小三档表)+ `INFO modules` + 站点同步

### V6 — 发布轮(v5.0.0 或先 rc,待 P4)
- [x] CHANGELOG 收口 + upgradegate 重测 ✅(2026-08-10 RC-READY F1-F3)
- [x] v5.0.0 完全发布 ✅(2026-08-11:40 crates 全链 + 三平台二进制 + npm @goliapkg/kevy + GitHub Release + 官网同步 + 装后 smoke 双通道;publish 链三修见 memory)

## post-v5(2026-08-11 起)

### P0 — SDK v5 真对齐 ✅(2026-08-11,merge `4ad82630` CI 绿)
- [x] 17 绑定 manifest 真 5.0.0 + vendored 原生 5.0.0 重建(vendorgate 29/29)+ 本机 11 门真绿 + mobilegate 六格设备门全 PASS;四个潜伏缺陷抓修(npm smoke -4* glob / nitro modulemap 配方脚本化 / expo ffi .so 漂移 / mobilegate booted 歧义)。渠道首发(npm 家族/NuGet/PyPI/pub.dev/maven/kevy-go repo/SPM 根 manifest)= 属主拍板项

### P1 — 元素粒度 COW(巨集合 rewrite 窗写停顿收口)【当前】
- [x] 设计轮 ✅(2026-08-11):正式 RFC `.claude/rfcs/2026-08-11-post-v5-element-cow.md`(A 案分段;Explore 全图供地面真相;分期 L→HS→Z,Stream 文档边界)
- [x] Stage L(List)✅(2026-08-11,merge `183bc8f3` CI 绿):Value::SegList 16K 元素段;盒上 Phase A 形状复测 352/666ms → **0.4/0.6ms 与 N 无关**,RSS 瞬态 341-392MB → ~2MB;perfgate-median 12/12 PASS(lpush cell -6.6% 在 floor 与波带内,观察项);finding=bench/FINDING-2026-08-11-element-cow-stage-l.md
- [x] Stage HS(Hash/Set)✅(2026-08-11,merge `f74e83c2` CI 绿):SegMap<V> 可扩展哈希(目录存索引/桶独立存放——首版目录内 Arc 共享被 100K 分裂测试抓出写分叉,已重设计);盒上复测:hash 窗内首写 1.2-2.1ms、set 0.1-0.3ms,20M 与 1M 无差,RSS 瞬态 ≈ 单桶;perfgate-median 12/12(hset +2.7/sadd +4.2 触碰面为正);crashgate midfile-resync 首犯 flake 入档(本地+重跑双绿);finding=bench/FINDING-2026-08-11-element-cow-stage-hs.md
- [x] Stage Z(ZSet)✅(2026-08-11,merge `f8acc77e`):设计改道——不 fork ranktree 内部(remove 再平衡是注 bug 高发面),SegZSet = 有序分段 Vec<Arc<RankTree>>(区间不交 + 缓存 max 路由)+ by_member 复用 SegMap<f64>,两石头零改动;盒上复测 0.7-1.0ms 窗内首写、20M 与 1M 无差;perfgate-median 12/12(zadd -1.7 波带内);miri 40min 超时被新测试撞出 → seg 套件按既有 time-box 先例 cfg_attr(miri, ignore)(unsafe 面已由平表示子集覆盖);finding=bench/FINDING-2026-08-11-element-cow-stage-z.md
- [x] 收口验收 ✅(2026-08-12,merge `79fbfe14`+`9fb65ae6`):**收口 soak 抓出第二轴 = per-tick 聚合(触段数×段体积)**,消融 16K→2K→512(1.86s→188ms→COW 份额 ≤65ms);双对照差分归因(无窗 50ms 底 / strings-only 有窗仍现 127ms@rewrite-finish)证残留属强制全 shard 同步 rewrite 的 finish 座位(S5-G 家族,auto 错峰免疫)——**COW-attributable ≤65ms ≤100ms 线 PASS**;512 粒度 perfgate-median 12/12;persistence 三语边界改写 + site 再生;finish 座位单列后续 attack 候选(BGREWRITEAOF fan-out 错峰)。finding=bench/FINDING-2026-08-11-element-cow-closeout.md
- [x] 顺手修一个 Fuzz 抓的存量真 bug(2026-08-12,merge `9fb65ae6`):kevy-config 尺寸字面量可乘出 >i64::MAX 的 u64,emit 裸整数后 lexer 拒收(roundtrip 不对称)→ parse_size 门口封顶 + 四边界回归测试

### P2 — forced-rewrite finish 座位(尝试一刀,被测量翻案,重新定名)
- [x] stagger 攻击已试并撤销 ✅(2026-08-12,`849f816c` 记档):按「多 shard swap 碰撞」归因实现了 BGREWRITEAOF fan-out 错峰(shard i 延迟 i×500ms)——复测显示 finish 已分离(三个 50ms 级小阶)但**单 shard finish 自己仍扛 183ms tick**,碰撞框架被证伪 → 分支未 merge 即删,翻案入 closeout finding Post-script
- [x] 单 shard finish 分解 + 刀 ✅(2026-08-12,merge `ad0ed0a4` CI 绿):三轮插桩定名——臂级计时锁定 tee-appended 283ms → 第一刀(GB 缓冲反应堆内联释放三漏,真隐患但非本座位,保留)→ 分步计时破案:**涓流工况 tee 永不排空,每次 rewrite 都走反应堆同步兜底 swap(S5 离线 SwapImage 只在 tee 空时启用),rename/fsync 撞 jbd2 提交窗即 ~300ms**;刀 = 终局小尾巴 tee 作为 SwapImage 的 tail 交 worker(append/fsync/hardlink/rename 全离线,dead-worker 回收 tail 走同步回退,epoll 保同步语义);复测最坏 tick 188ms → **50.5ms 噪音底,8 次强制 rewrite 零超线,PASS**;crashgate 31/31 + perfgate-median 12/12 + CI 绿;persist_worker 按 500 LOC 拆 job 体到 persist_jobs。finding=bench/FINDING-2026-08-12-rewrite-finish-offthread-tail.md

### P3 — S2:appendfsync=always 的 CQE-gated 回复【当前】
- [x] 设计轮 ✅(2026-08-12):正式 RFC `.claude/rfcs/2026-08-12-s2-always-cqe-gated.md`(Explore 全图地面真相;epoch 门控回复持有 + 排空后 fsync 免 SQE 链接;**两个关键地面事实:queued 模式下同步 fsync 对空文件 = 潜在正确性陷阱必修;crashgate 对反应堆侧 always 零覆盖,server-always cell 必须先行**)
- [x] 实施 ✅(2026-08-12,feature/s2-crashgate-server-always):①-④ 全落地,机制按实现轮改道为**记录水位线**(queued_seq / durable_watermark,免 OP_AOF_FSYNC 新 tag;RFC §6 Resolution)。stamp 面 ×3(recv 批 / bigbulk feed / **B2-alt bareset——Explore 图漏的第三面**);跨 shard held_responses;run_swap 补 rename 后目录 fsync。步② 陷阱修红-绿验证。
- [x] 验证 ✅:crashgate 32/32(盒 uring 门控路径;新 server-always cell 基线先绿于旧路径)+ kevy-persist 70/70 + kevy-rt 50/50(Linux)+ 分支 CI 绿(availgate 二犯 flake 入档 bench/.flake-archive,--no-aof 不可达,rerun 绿)。**A/B(盒 ext4):SET c50 353→8,273 rps(+23×,组提交);c1 序贯 −29~49%(fsync+ring 往返,RFC 预告的语义代价);三组数据三角证门无泄漏(tmpfs 51µs/op vs ext4 7ms/op 全 fsync-bound)**。finding=bench/FINDING-2026-08-12-s2-always-cqe-gated.md

### P4 — availgate failover 收敛 flake 根因(部分:两真缺陷已修,**三犯证明根因不完整**,续 P6)
- [x] 根因(首稿「双主瞬态」判读被 forensics 复核推翻——n1 的 PRIMARY 是启动自选举):**① feed generation 是计数器,跨节点数字必然碰撞**(fresh 全叫 1;启动选举与 failover 提升都叫 2),stale cursor 穿过 gen fence 进 offset 别史;**② pump 的 caught-up `>=` 遮蔽了文档承诺的 forked-history snapshot ship**——同代 ahead cursor 永久假 caught-up(心跳照发 link up 不收敛 = forensics 原样签名)。
- [x] 修复:generation = 随机 53 位**历史身份**(RESP i64 + JS Number 2^53 双线安全;fresh/unclean boot 与每次 bump 都重抽;mismatch 一律 Resync,身份无序);caught-up 改精确相等,ahead cursor 落入 Future 臂 ship snapshot。**JS 2^53 线是实战教训:63 位首版被 ts conformance 抓出 Number 精度截断自 Resync 循环**。
- [x] 验证:新 ahead-cursor e2e(旧码上 wedge 为 ping-only)+ feed/feed_meta/embedded 身份单测 + gen 字面断言全库迁移(feed_cdc/wrap_parity/embedded/e2e ~20 处)+ **盒上 availgate ×10 全 PASS**(带轮间端口 settle;此前 ×10 的 5 FAIL 是循环器自身固定端口 AddrInUse 工件)+ repligate PASS。

### P5 — S3:epoll/kqueue 反应堆的 AOF writer 线程【当前】
- [x] 设计轮 ✅(2026-08-12):正式 RFC `.claude/rfcs/2026-08-12-s3-epoll-writer-thread.md`(Explore 全图;**关键地面事实:epoll=kqueue 共路径一条 Shard::run;queue 机制 cfg(unix) 可直接复用、O_APPEND 下 offset 只是记账、顺序 write_all 与 uring positioned write 磁盘等价;S5 off-thread swap 封锁判据是 queued_mode() 非反应堆种类=按构造自动解锁;S3 现状零专属门——crashgate cell S/tailgate 全 auto 反应堆,盒上即 uring**)。机制=per-shard writer 线程(persist_worker/bio 先例)+ S2 三件套 epoll 对偶(Conn.held_watermark + flush_conn 单门点覆盖 7 个写点 + cfg 放宽)。
- [x] 实施 ✅(2026-08-12,feature/s3-epoll-writer-thread):①-④ 全落地。实施期三收紧(RFC §6):dead-lane SendError chunk 经新 `Aof::requeue_front` 回插队首(order+offset 回归钉死)/ CONFIG SET appendfsync 先 settle lane(owner 句柄 flush 与 clone 在途交错缝)/ 门 park 撤 write interest 防 level-triggered 自旋。S2 三件套全平台化(always_hold_w0/held_responses/flush_held_responses 摘 cfg(linux),双驱动共用)。
- [x] 验证 ✅:crashgate 33/33 双 host(server-always 双 cell:盒 auto 37,896 + epoll 37,813 acked 全存活;macOS kqueue 423/409)+ persist 72/72 + rt 50/50 + **盒上 A/B(ext4,KEVY_IO_URING=0,always):SET c50 478→10,540 rps(+22×组提交);c1 391 vs 384 持平且 fsync-bound=门无泄漏** + perfgate-median 12/12(uring 面零回归)。PR 测试矩阵(强制 epoll)整体即 lane 实弹面。finding=bench/FINDING-2026-08-12-s3-epoll-writer-lane.md

# 开放工作流(2026-08-13 重整,属主令「把这些问题都先合理地补充到 flow 然后全部解决」)

v5.1.0 已发布并 dogfood 回归通过。此前散在几个旧 arc 里的未勾条目,按
**归属**重排如下——每条要么我做、要么属主拍、要么写死 OUT,**不留含糊**。
线性,从上往下。

## F1 — 陈账清理(账目落后于实现,先对齐再谈剩余)✅
- [x] `V1 余项` 三条经核实**已实现**:`kevy-scalar/src/regex_engine/`(1260 行,fork 自 spg ERE)+ `regexp.rs` + `md5.rs`,全部接进 dispatch,`cargo test -p kevy-scalar` 25+1 绿,funcgate PASS。ROADMAP 记的是旧状态。
- [x] `V0 合并态全门禁` 与 `T9 allocgate/compressgate/envelope` 同属"当时已绿、只差 CI push"的历史条目;5.1.0 的发布本身就是这些门在 CI 上真绿的证据。

## F2 — 拍板点 ①③④ 终判 ✅(2026-08-13)
- [x] ③ regexp = **fork spg + 改壳**(已实施,1260 行 `regex_engine/`);理由与 CLAUDE.md 的 LOC-WAIVER 第二类同源:重写一份已测引擎核心是注 bug 高发面、可读性收益为零
- [x] ④ md5 = **自研**(`md5.rs`);引 crate 会破零依赖铁律,而 md5 无演进面
- [x] ① bar = **B-1 双线终值**:线①能力 `subset-foldable ≥ 82%`(实测 82.5%,ratchet 只升不降,分母是本弧声明要服务的子集而非 89 文件);线②诚实 `wrong == 0`(**硬线**,实测 89/89 零 wrong)。章程的「89×80%」是测量前写下的句子——分母吃进了 52 个永久具名拒的探针,按它设线只会逼出假绿。RFC §7 Resolution 已记,funcgate 措辞已换终值

## F3 — V5 产品面 ✅(2026-08-13)
- [x] 容量计算器 + `INFO modules` — **核实为陈账,早已完成**:`INFO Modules` 段在 `info_sections.rs`(alloc 实现 / tiering 状态 / 命令面),容量计算器在 `kevy.golia.jp/capacity/` 线上(公式 `max data:RAM ≈ value_size / (96 B + key heap)` + 交互输入 + 实测对照)
- [x] **边界页 `docs/boundaries.md` 三语已写并上站**(Reference 区):四条线(Redis 契约不归我们改 / 含义与执行计划留在应用 / 拓扑声明非发现 / 网络可信)+ 按领域的拒绝表(每条写为什么和改用什么)+ 函数面现在地(能力线 82.5% ratchet、诚实线 wrong==0 硬线)。此前拒绝清单只活在 `.claude/`,用户面一处都没有——这才是这条的真缺口
- [x] **四区重构:我判定不做,理由在此**。现有五区是**任务导向**(上手/运行/数据/嵌入/参考),提议的四区(Core KV / RDS 模块 / 运维 / 迁移)是**产品架构导向**。读文档的人问的是"我要做 X 怎么做",不是"这属于哪个产品分区";把可用的任务导向结构改成架构导向会让读者更难找到东西。那条目里真正有价值的半条是边界页,已做

## F4 — 属主硬闸(已查清各渠道**具体**卡在哪,不是笼统"待拍板")

技术面全部就绪:14 个门全 5.1.0、vendored 字节 5.1.0、ffigate 30 格 /
TS 50/50 / python 真跑通。剩下的阻塞按渠道分三类:

- [ ] **缺凭据(仓库 secrets 只有 `CARGO_REGISTRY_TOKEN` / `NPM_TOKEN` / `DOCKERHUB_*`)**:NuGet 无 API key、PyPI 无 token、pub.dev 无 credentials、maven 无签名密钥。这四个不是我能补的
- [ ] **缺一个命名决定**:`bindings/ts` 与 wasm 包同名 `@goliapkg/kevy`。**npm token 是有的**,所以 node / electron / expo / nitro 四个包今天就能发,只有 ts 被这个重名卡住
- [ ] **缺一个仓库结构决定**:Go 模块要独立 repo(`go.mod` 已声明 `github.com/goliajp/kevy-go`,该 repo 不存在);SPM 要**仓库根**有 `Package.swift`(现在在 `bindings/apple/KevyKit/`),等于要决定这个 repo 是否同时充当 SwiftPM 包

三类都要属主动作(给凭据 / 定名字 / 定结构),我这边不再有可推进的余量。

## F5 — 写死 OUT(不再作为"开着的"出现)
- 断电级 fsync 验证(dm-flakey):kill -9 面已覆盖顺序性,电源级留研究项
- bounded SPSC 环 / write+fsync SQE 链接:S2/S3 的优化观察项,无正确性面
- macOS 并行 spawn-timeout:A/B 证明与代码无关,环境 flake
- 引擎侧 AUTH/TLS:`feedback-kevy-auth-tls-never`,愿景变了也不解锁
- 写时计算列(拍板点 ②):RFC 已定"V1 不做,V4 后议"
- T9「判定这次尝试算不算 v5」+ SME 口径 PG 复测:**v5.0.0/5.1.0 已发布,这个判定被事实回答了**;PG 对比作为独立选题保留,不挂在 v5 判定上

## 绑定全面跟版 5.1.0 ✅(2026-08-13,属主令「要绑定到 5.1,因为 5.0 有严重缺陷」)

**我的漏**:5.1.0 的 bump 只动了 Cargo manifest,14 个绑定还声明 5.0.0——**其中两个是用字节声明的**。这次这不只是不自洽:5.0.0 的编码器会写出自己解码器拒收的压缩帧,所以带 5.0.0 引擎的门就是在发这个隐患本身。

**真正陈旧的是引擎字节(其余只是版本字符串)**:nitro track 进 git 的两个 jniLibs(arm64-v8a / x86_64)自报 5.0.0 → 用 `packaging/android/build-ffi-jnilibs.sh` 重建;KevyKit 的 `Kevy.xcframework`(ios/ios-sim/macos)重建并经 `prepare-native.sh` 重新 vendor 进 nitro;**三者现在都自报 5.1.0,vendorgate 29/29 PASS**。

**版本声明面 26 个文件**:12 个 manifest(npm×6 / pyproject / pubspec / 两个 csproj / server 二进制 npm 包 / wasm 包模板)+ **包间 `@goliapkg/*` 互 pin**(否则 5.1 的门会拉到 5.0 的同伴包)+ expo gradle 的 `version` 与 `versionName` + tauri 插件 crate 版本 + **python 活的 `__version__`** + README/PUBLISH-FORM 里"tracks kevy 5.0.0"的声明。**故意没动**:第三方 lockfile 里的 5.0.0;tauri 插件的 path 依赖(从来没 pin 版本,一直在构建 5.1.0)。

**验证不是假 bump**:ffigate-contract 30 格(六门×五行)/ TS 套件 50/50 / python 门从树内 import 自报 5.1.0 且对新构建引擎真跑通读写 / vendorgate / locgate / commentgate / rootgate / **develop CI 绿**。**发布仍是属主的**:除 `@goliapkg/kevy`(wasm,已 5.1.0)外,其余绑定包在各 registry **从未发布过**(npm 四个包 E404、PyPI/NuGet/pub.dev 全 404),所以没有任何陈旧产物在用户手里;首发决策照旧留属主。

## v5.1.0 dogfood 第二轮 ✅(2026-08-13,smix 回执 b)

**他们要的那一节已写并上线**:`docs/upgrading-5.0-to-5.1.md` 三语加"如果你是嵌入式使用者"——只写他们说的那两句 + "哪些配置项对嵌入式无意义"清单。**两句是实测不是推断**(拿已发布 crate 跑嵌入式路径:5.0.0 写 201 键含一个 200KB → 5.1.0 读回 201 → 5.1.0 追加 201 → 5.0.0 重开读回 402,双向 clean replay,同一程序对两版本无改动编译)。他们的整个调用面=`Config::default().with_persist(dir)` + `Store::open` + get/set/del/keys,`grep -niE "getex|pexpire|expire|ttl"` 在其源码上命中 0 处 → 4.1.1 的 GETEX 缺陷与 TTL 绝对挂钟契约都够不到他们。

**他们的两条规则又替我们抓出三处**:① **管道吃判决**审计:全部 `bench/*gate*.sh` **没有真缺陷**(唯一 `$?` 站点是 heredoc 取 python 退出码),但收紧一处**隔空读 `$?`**(availgate seeder 的 `[ $? -eq 0 ]` 隔着三行注释,改成 `|| fail` 挂命令上,与同文件姐妹 seeder 一致);**判断:不给全部脚本盲加 pipefail**(`grep | head` 的 SIGPIPE 会变非零 = 拿一个坑换另一个坑)。② changelog 页上站后 `check_links` 立刻抓到**29 条死链**(changelog 引用的是仓库产物:RFC/workflow/finding/一条笔记本绝对路径)→ 渲染器支持链接映射**拒绝**一个链接(留文字去锚点)。③ 同一次扫描顺出**存量缺陷:26 个页面的仓库链接指向 `blob/main`,而本仓库没有 main 分支**,全 404(含每处"见 bench/REPORT.md"和 cookbook 的 shop.sql);三个来源各自硬编码,现 256 条全指向真实 ref(抽验 200)。**取舍报备**:site-commands 门把 changelog 里 v2.0.15 的基准输出转录(`=== PING:`/`PONG`)当命令跑而红 → **changelog 排除出该门**,理由=changelog 的代码块是证据不是指令,拿今天的二进制执行历史转录测不出东西。回信 `/tmp/kevy-reply-to-smix-2026-08-13-b.md`。

**这轮的元教训**:新东西上线最大的价值经常不是新东西本身,而是**它把老东西送进了一道之前没扫到它的门**(29 条新死链 + 26 页存量 404 是同一次扫描出来的)。

## v5.1.0 dogfood 回归 ✅(2026-08-13,smix 回执)

**smix 已升 5.1.0 并双向验证**(真实跑了几个月的 5.5MB store,非 fixture):升级方向 5.1 与 4.1.1 各读同一批 4.1.x 字节、**162 条三类记录逐字段相同**;降级方向 5.1 写入副本后 4.1.1 重开、**replay 300 条 clean** —— 我们指南承诺的降级窗口第一次有了真实用户机器上的证据。嵌入式一侧 4→5 无 API 破坏(编译即判据)。他们全量 preflight 28 门绿,落 `043b5e51b`。**vlog/压缩隐患对他们结构上不成立**(`kevy-embedded = "4.1"` caret 走不到 5.0 + 只用 `Config::default().with_persist()` 没开压缩)——**我们通知写法的问题:按"所有人都用全部功能"写的,下次先按对方实际打开的能力面裁**。

**他们报的发布物缺陷已修两处**:① 他们拉不到 4.1.1 的 CHANGELOG——仓库公开、develop 分支 200,但**按惯例会去的 master 落后 1216 提交、最新条目停在 v3.8.0**,任何按"stable line"找 4.1.x 的人拿到的是没有 4.x 条目的文件;master 已快进,发布流程补了这步。② **发布说明上站** `https://kevy.golia.jp/changelog/`(此前是软 404 返回首页还给 200),从 CHANGELOG.md 生成并进站点 `--check` 门。回信 `/tmp/kevy-reply-to-smix-2026-08-13.md`(含 4.1.1 的实质答案:`Store::getex` 记相对 PEXPIRE → replay 重锚 → GETEX 做缓存续期的键跨重启永不过期)。

**他们送的方法论已进门禁纪律**(`.claude/rule/hygiene.md`,注明来源):**判据不许在空数据目录上成立** —— "拿一个空数据目录跑它,还会给出同样答案吗?会的话这条判据没在验存储"。按它审计自家门抓到并修两处空判据:`crashgate` 的 `recovered >= synced` 在 synced=0 时恒真(已要求非零,**红-绿实证:同一零写入输入旧 guard 过、新 guard 红**);`upgrade-interop` 场景 D 的 `0 = 0`(已先断言种子非空)。推论也写下:**"必须不存在"型断言天生空判据**,必须与"必须存在"的内容断言配对。crashgate 改后盒上仍 PASS。

## v5.1.0 ✅ 已发布(2026-08-13)

**全渠道到位**:crates.io 38/40 crate 至 5.1.0(kevy-client / kevy-client-async 走独立 2.2.0 轨)+ npm `@goliapkg/kevy` 5.1.0 + GitHub Release v5.1.0 非草稿(三平台二进制 + sha256)+ 官网 kevy.golia.jp 同步(三语升级指南 200;**playground wasm 重建为 5.1.0**,真 Chrome 13/13 验过,线上与本地 sha256 逐字节一致)。tag `v5.1.0` 指向 develop `e08f6d09`;**master 已快进到发布态**(此前落后 1216 提交,GIT-FLOW 那句"每个 tag 指向 master"重新成真)。**装后 smoke 双通道**:`cargo add kevy-resp@5.1.0` 编译运行 / 发布二进制 sha256 校验 + 起服读写 + `kevy_version:5.1.0`。smix 通知已投 `/tmp/kevy-notice-to-smix-2026-08-13.md`(头条=5.0.0 的 vlog 冷值读挂隐患,升级本身即修复)。

## v5.1.0 target(属主令 2026-08-12:nodefer 到工业级能用,然后全面更新)
总计划=`.claude/plans/2026-08-12-v5.1-target.md`(IN 六项全闭才请属主扣发布扳机;OUT 已写死)。

### P6 — 【5.1 关键路径】availgate failover 收敛 wedge:插桩复现弧 ✅ CLOSED
- [x] 探针面 ✅(2026-08-12,`KEVY_DEBUG_REPL_TRACE=1`,保留为常设调试面):promotion bump 时刻(bump 前 feed 位置 + 全部已挂 cursor 状态)、握手 claim vs feed 位置、两个此前**零日志**的 pump 臂、ship begin/end、AckSent 卡住检测、runner 会话事件;全部带统一挂钟 ms 戳,三节点 log 可交织成一条序列。availgate.sh 加 `KEVY_AVAILGATE_KEEP`(EXIT trap 此前会删掉自己的犯罪现场)。
- [x] 破案 ✅:**争用循环不是配方**(修前探针版 28 轮争用未复现)——窗口只有约一个 tick 宽,确定性 in-process 复现才破的案。根因 = generation fence 的 fresh-cursor 例外:`gen 0/offset 0` 被当作"空 replica"采纳并从 0 流帧,而促升 bump(以及带数据的非干净重启)会**换掉整个 source**(帧全弃、next 归 0)却保留 store → 采纳后的 cursor 恰好"已追平"、永不 ship,心跳照发 link up。促升窗口(写闸随 epoch 开、各 shard 自己 tick 才 fence)内的写入被 bump 销毁 = availgate 等的那条 postfail。
- [x] 修 ✅(merge `caa5f346`):**gen 不匹配一律 ship snapshot,无 fresh-cursor 例外**——无 claim 的 cursor 不等于空 replica(runner cursor 每次 respawn 都归零),帧流只能增改、删不掉 replica 多出来的键,只有快照能替换键空间。同 Redis 的 full-resync 模型。
- [x] 验证 ✅:新 e2e(红→绿,两种红脸:停滞 + 静默分叉)+ replication 26/26 + cluster-rw + rt/replicate/persist 全绿;**盒上争用循环 20/20 绿**;repligate PASS + crashgate PASS;分支 CI 全绿。finding=bench/FINDING-2026-08-12-availgate-promotion-window-wedge.md;flake 档案已结案。

### P7 — 5.1 收口面
- [x] 混版本升级互通 ✅:`bench/upgrade-interop.sh`(A/B/C/D 四场景,对打真 v5.0.0 二进制)全 PASS。**场景 C 修前是红的**——带 5.0 计数器 gen sidecar 的 replica 重指 5.1 primary 会保留升级前的键(fork 没丢弃);修后每 shard 恰好一次 snapshot resync 自愈。即 P6 的修复正是 5.0→5.1 升级自愈路径的前提。
- [x] CI infra 两小刀 ✅(merge `57227fbe`):clientgate npm install 三次重试 + `--fetch-retries` + 失败时打完整日志(此前静默吞掉,两步之后才以 module-not-found 现形);C# harness 的 FreePort 探测→bind 碰撞按 spop 配方换端口重试。
- [x] tailgate epoll 观察轮 ✅(不设门):同日同构建 A/B——uring 双 cell 双 bar 仍 PASS;epoll mixed p999 4.68ms(优于 uring 8.15ms)但 tick gap 440ms(10×);firehose p999 117ms / gap 774ms 超线。mixed 的背离**单列为待测项而非解释**。finding=bench/FINDING-2026-08-12-tailgate-epoll-observation.md。**仪器教训**:首轮忘了 `TMPDIR=$HOME/captmp`,落到 32GB tmpfs 上被写满 → 探针全 reset,而门把空值渲染成四条超线 FAIL(空测量长得像失败测量);这个陷阱 FINDING-2026-08-09 早写过。
- [x] 文档 ✅:persistence ja/zh 同步 + upgrading-5.0-to-5.1 三语(含新增的 `-LOADING` 窗口说明)+ CHANGELOG 5.1.0 + site 再生 107 页;doc 四门绿。
- [x] publish 链拓扑序 ✅ 变成门禁:`tools/check_publish_order.py` 进 CI——含 **dev/build 依赖**(v5.0 那次 kevy-cluster-rw dev-dep kevy-rt 的真陷阱已红-绿实证),并交叉核对 workflow 里两份手维护列表。
- [x] 发布彩排 ✅:workspace bump 5.1.0(40 crate 同步,含精确 pin 与对齐 pin)+ release-profile 全 workspace 构建 EXIT=0 + npm 版本从 tag 推导已核 + perfgate PASS 12/12(漂移 ±2% 内)+ develop CI 实测绿。**三栏报告=`.claude/plans/2026-08-12-v5.1-rc-ready.md`,只剩属主扣扳机**(tag→publish 链→npm→Release→官网→smix 通知)。

### P8 — epoll/kqueue tick 节拍(P7 观察轮点名的下一步测量)✅ CLOSED
- [x] 破案 ✅(merge `8cd450f2`,CI 绿):tick 把"没变过的 appendfsync"当成待应用的策略切换,而切换协议按设计先排空 offload driver——poll 反应堆上那是忙等 writer lane 排空,于是**每 100ms 同步排空一次**,火管工况下就是大半秒。修=只有策略真变了才算待切换(uring 的 deferral 半边未动)。**不是 5.0 回归**:lane 与排空纪律都是 5.1 新增。
- [x] 定位全程靠测量,三步 ✅:① `reactor_ticks_total` 把最高水位 gauge 变成可读的一对——**节拍其实健康(38/40Hz),证伪了我自己"约 2Hz"的读法**(那条已在 finding 里撤回)② `KEVY_DEBUG_SLOW_ITER_MS` 相位分解把停顿钉在 tick 相且 `events=0` ③ 再拆 tick 体,891ms 的迭代里 884ms 在 `apply_live_runtime_config` 一个调用里。
- [x] 验证 ✅:epoll firehose 最坏反应堆停顿 790→34-42ms、最坏客户端往返 862→14-16ms、p99.9 123→10ms、超 100ms 迭代 97-106→**0**;**tailgate 在 poll 反应堆转绿**(此前四数红三);crashgate PASS;perfgate 面未触碰(改动只在 tick 相)。finding=bench/FINDING-2026-08-12-epoll-tick-drained-the-lane.md。
- [x] epoll cell 正式设门 ✅(2026-08-13,属主「你来定吧」后我拍的):tailgate 加 `mixed-epoll` / `firehose-epoll` 两格,同 100ms 线;设门前跑三轮取重复性证据(firehose-epoll 中位数 37/61/69ms,最坏单跑 79.8ms),这个近 2× 的中位数抖动写在调用处,红了先当信号读。门是盒上门不在 CI,墙钟翻倍只落在发布前那一次。同轮加固该门两处:**拒绝在 tmpfs 上出数**、**「没有测量」成为独立 verdict 与独立 exit code**(此前 `${x:-999999999}` 默认值把空探针输出渲染成四条超线 FAIL)。
