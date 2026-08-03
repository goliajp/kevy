# R1 设计轮:kevy-alloc residual —— 两个 master 都要伺候的分配路径

> 接住点 = `feature/v5-memory` 分支 `17f85688` 的 v8 closing ledger。
> residual:集合写(hset 最差角 −18.6%)的分配器 self-time 17.3%(pop_slot 7.6 +
> alloc 5.3 + dealloc 4.4)vs glibc tcache 10.1%——每次位图 alloc/free 都触到
> span 元数据的远 cache line,集合写每 op 多次小分配放大了它。
> **已死的天真修法**:heap-local free-slot 缓存——远线消了,densification 毁了
> (M3 1.98× → 2.38×),吞吐还没赢。任何下一设计必须同时伺候两个 master:
> **回收要热(近线),分配要有位置感(最低 span 优先,让高 span 腾空还 OS)**。

## 候选对抗(ledger 三形 + 本轮新增一形)

| | 1. 最低 span 槽缓存 | **2. per-word bit 批取** | 3. 接受 trade | 4. 小类 slab 页 |
|---|---|---|---|---|
| 机制 | 只缓存当前最低 span 的空槽 | claim 一整 word(64 槽)进 heap 本地,逐 bit 发放,满/tick 写回一次 | 不修,−10~19% 集合写换 −17% 常驻 | 集合写的已知 size class 专开 slab 页,页首 free-list 头(同线) |
| 远线触达 | 分配侧消,**free 侧仍在**(4.4% 原样) | 分配+释放两侧都摊 64:1 | 原样 | 页内近线;跨页仍远 |
| 位置感知 | 保(定义上只碰最低 span) | **保**(claim 时选最低 span 的最低空 word,粒度从 bit 粗到 word) | 保 | 粗化到页粒度(整页归属 densify) |
| M3 风险 | 低;缓存失效协议是复杂度所在(最低 span 会变) | **占用计数滞后 ≤64 槽/word**——span 空判定延迟,预计中性,须实测 | 零 | 页级驻留:半空 slab 页拖 densify,风险与死机制"per-heap pool"同族 |
| 复杂度 | 中(失效协议) | 中(claim/写回状态机,每 heap 每 size-class 一个在飞 word) | 零 | 高(新页型进 span 模型) |

**定稿倾向:候选 2(per-word bit 批取)为下一实验。**
理由:唯一同时摊薄**两侧**远线的形状(dealloc 的 4.4% 只有它管);位置感知不是被绕开
而是被**粗化**(最低 word 代替最低 bit——densification 的目标是 span 级腾空,word 粒度
不伤它的语义);批回与 R2 的合成顺手(出窗迁移是同段成批 free,天然整 word)。
候选 1 记为备选(若 2 的占用滞后实测伤 M3);候选 3 是 fallback(SME 权衡,归属用户);
候选 4 记入负结果档预留(与已死的 per-heap pool 同族,不试)。

## 实验形态与判据

- 实施在 `feature/v5-memory`,单 commit 可 revert;fuzz 先行(claim word 在飞时
  Heap::drop / 跨 size-class / 空 word 写回的边角——v8 的 fuzzer 分钟级抓过 pool 泄漏)。
- **结构性判据**:alloc/dealloc 的远线触达从 per-op 变 per-64-op(perf-record 的
  span-metadata 符号 self-time 掉出双位数);M3 ≤ 1.98×(不许回 2.x);12 角不退,
  hset 角向 glibc 差距收敛。
- **纪律**:这是设计轮产物的一次实验,不是 polish 链的开始——若 2 与 1 都不动针,
  回到 decomp(重拆 hset 的分配形状本身,而不是第三个缓存变体)。

## 顺带记一笔(不属于分配器)

集合写的"几次小分配"里有多少可以**不分配**?(SmallHashInline 的 inline 界、
hash 节点的 arena 化)——这是值表示的单元,不是 R1 的;若 R1 实验后 hset 角仍是
最差角,这条升格为独立研究项。
