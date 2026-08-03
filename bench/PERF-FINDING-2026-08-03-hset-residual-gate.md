# hset 剩余税 perf-record 复判 — gate SAYS NO,residual 改性(Pre-Phase-B)

> residual 设计轮第一步。合成树(r1-eval)双 bin,lx64,hset -P256
> 负载,dwarf call-graph,~250k samples/side,strip=none 全符号。

## 判定数据(self-time)

| 侧 | 分配器符号 | 合计 |
|---|---|---|
| alloc-ON | `kevy_alloc::Heap::alloc` 7.73% + `Heap::dealloc` 3.41% | **11.1%** |
| alloc-OFF | glibc `malloc` 6.89% + `cfree` 3.18% | **10.1%** |

**分配器自身时间差 = 1.1pp**(v8 时代 17.3% vs 10.1% = 7.2pp —— 
per-word claim 把它消掉了)。M1 上 hset 吞吐差 -12.1%:**1.1pp 的
代码时间解释不了 12% 的吞吐**。

对照面:`run_uring` 聚合 ON 23.95% vs OFF 21.00%(其中 spin PAUSE
16.24 vs 13.39,+2.85pp)—— ON 侧每 op 更慢发生在**分配器符号之外**,
等待与其余 store 符号摊薄吸收了税。

## 结论(Pre-Phase-B gate)

1. **攻击面 < 双位数 pp** → 按方法论 §9 双 gate:对 kevy-alloc
   **代码路径**的任何下一轮 polish 无靶,禁止启动(这是 gate 第三次
   实战说 NO,前两次见 §9 维护记)。
2. **residual 改性**:从"分配器代码的元数据远线"改为**"分配出的
   对象布局/局部性税"** —— kevy-alloc 返回地址的密度/相邻性
   (bump-in-span vs glibc 的 tcache 复用热地址)使 store 侧
   hashtable/节点访问的 cache 行为变差,税摊在全部使用侧符号上,
   任何单符号都不过双位数。
3. 候选方向(设计轮素材,未验证):近期释放槽的**有界局部复用**
   (与死掉的 LIFO 热缓存的区别:按 span 内地址序而非时间序,保
   致密化)/ size-class 内 bump 指针回退策略。**任何尝试必须与 M3
   同轴复测**(热缓存 -137MB 蒸发四轮的教训)。
4. 验证路径(先于任何实现):cache-miss 计数对测(perf stat
   L1-dcache-load-misses / LLC-misses,ON vs OFF,hset 角度)——
   若 miss 差与吞吐差同量级,改性成立;不同量级则重新 decompose。

## 环境记档

lx64 `kernel.perf_event_paranoid` 由 4(默认)放宽至 1(runtime,
未持久化)—— profiling 权限,不影响他人服务;后续 profile 轮直接可用。
