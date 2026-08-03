# R1 完全体评估 — kevy-alloc × v5-T 系列合成,决策材料

> v5 总案 R1 的收口评估。分支 `feature/v5-memory`(31 commits,per-word
> claim 止)与 develop(T 系列全线,83 commits)合成于评估分支
> `r1-eval`;lx64 全 M 集实测。**merge 决策归属主 —— 本文是材料,
> 不是决定。**

## 一、共存性(合成工程账)

- 合成**零源冲突**(唯一冲突 = kevy/Cargo.toml 依赖块版本推进);
  kevy-alloc 只触自身 crate + kevy bin 挂钩,与 T 系列(kevy-store
  segrows/tier、kevy-window、kevy-text cold)正交 —— 石头分层的红利。
- workspace 全测绿(alloc on/off 两态);M3-identity **EXACT** 保持
  (mapped == live+…+overhead 恒等式);M4 reclaim / M5 foreign-free /
  M6 class-cap / M8 unsafe-ratchet 全绿。

## 二、性能账(M1 KV A/B,off=ref vs on=cand,n=5/side 交错,lx64)

| 角度 | on vs off |
|---|---|
| pinned_cluster_get | +0.4% |
| pinned_cluster_set | -3.4% |
| pinned_compat_get | -3.7% |
| pinned_compat_set | -1.0% |
| legacy_8sh_get | -3.9% |
| **legacy_8sh_set** | **-8.5%** ✗ |
| **legacy_8sh_incr** | **-9.5%** ✗ |
| **legacy_8sh_sadd** | **-10.2%** ✗ |
| **legacy_8sh_hset** | **-12.1%** ✗ |
| legacy_8sh_lpush | -4.7% |
| **legacy_8sh_zadd** | **-14.3%** ✗ |
| zalg_zinterstore | **+8.5%** ✓ 反超 |

与 v8 终账/per-word 轮定性一致(hset -12.1 vs 分支 -12.5):
**T 系列共存没有引入新互作**,residual 仍是"集合写小分配元数据
远线"一族。读族/管线/集合代数在 alloc 下持平或反超。

## 三、内存账

M3:**1.98× vs glibc 2.40×(-17% 常驻)**,2M×400B/512MB 形状
(分支上验证,合成树 identity 恒等保持;M3-scaling / rss-residual
的 envelope 复测 = merge 决策后事项,PENDING 行在 allocgate)。

## 四、决策清单(归属主)

- **A. merge + 默认关**(feature `kevy-alloc` 门控,现状形态):
  主线获得可选"内存优先模式";零默认风险;集合写税(-8~-14%)与
  内存收益(-17%)都成为部署方的显式选择。工程上今天即可。
- **B. 分支冷藏**:residual 设计轮(hset 剩余税 perf-record 复判、
  sadd/zadd 归因 —— 均候选在案)有产出后再议。
- **C. 写族全绿后 merge**:开放研究;八个死机制在案,天真修法已知
  破坏 M3,成本无界。

先例锚(v5 愿景 memory):"接受集合写 −10~−19% 换 −17% 内存,
SME 取舍归属主"。评估者按 A 最符合 v4 以来的 feature 门控惯例,
但不代拍。

## 五、残余开放项(与决策无关,记档)

- M2 pubsub A/B **= 0.974 PASS**(ON 18.01M vs OFF 18.50M msg/s,
  floor 0.92)—— v8 的 0.858 / per-word 轮的 0.847 在合成树上大幅
  收窄。诚实记:OFF 侧仅 2 样本、spread ~10%,0.974 的精度有限,
  但方向明确好于 0.85 时代;若 merge 决策需要更细,加样即可。
- allocgate-mem runner 时序修(欠,分支账内)。
- hset 剩余税 perf-record 复判(Pre-Phase-B gate)= residual 设计轮
  的第一步,不论 A/B/C 都值得。
