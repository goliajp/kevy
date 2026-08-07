# v5 研究收扎实轮:perf / disk / stable 三轴平衡(2026-08-07)

> 属主定向:在工业转轨之前,把研究面做扎实 —— **perf / disk / stable
> 都拉到目前有可能的最好的平衡**。
>
> "平衡"的操作定义:三轴各自有实测的当前值与已知机制;本轮的任务是
> ① 把**缺测的组合态格子**补上(单轴数据拼不出平衡点)② 把每轴**还能
> 收的最后一刀**收掉或如实证伪 ③ 交出一个**平衡点配置**(默认值组合)
> 及其三轴代价表,直接喂 P1/P2 拍板。

## 已知的三轴现状(全部实测,单轴口径)

| 轴 | 当前值 | 已知机制 | 未收口 |
|---|---|---|---|
| perf | KV/cluster 绿;集合角 −10~−14(alloc ON,median);M2 0.83-0.88 | 快路径每调用成本(owner 线程 +10pp);三刀已落 | B'(单发信封池化)未试;alloc ON/OFF 组合态角表缺 |
| disk | vlog amp 0.01×(同值)/ 2.2×(templated,dict);冷读 p99 128µs 带内 | corpus 压缩全弧;压实级+文件级熵表 | compact 阈值(50%)/步长(256)是拍的不是测的;AOF rewrite 与 tiered 负载互动未测;realistic 语料稳态 amp 未测 |
| stable | zadd >3s 停顿已根治(drain 预算);恒等式 EXACT;活性契约绿 | 心跳探针(未机制化);wedge 类问题的教训在案 | **长跑 soak 从未做过**(frag 漂移/小时级);p99.9 无口径;组合态 crashgate 未复跑 |

**中心缺口:两个 ON(alloc + compress)从未同测。** T9 原句就是
"capacity envelope 复跑(alloc/compress ON)" —— 至今没跑过。

## 实验列表(R-A 为地基,先做;B/C/D 盒上串行;R-E 收口)

### R-A 组合态基线(地基,无它则平衡无从谈起)
1. **envelope 全刻度 @ alloc ON + compress ON**(compress 已默认在 vlog;
   加 alloc feature 构建):容量比 / B2 冷读 p99 / B5 amp / B8 预算 /
   frag —— 与 alloc-OFF 版逐格对照。
2. **perfgate-median @ 同一构建**(N=3):完整角表,与 alloc-OFF 基线、
   与上一发布口径两套对照。
3. **tailgate 原型**:混合负载下 PING p99.9 + reactor 单圈上界
   (心跳探针脚本化,不改引擎)。
   产物:组合态三轴一张表 = 平衡点的坐标原点。

### R-B perf 轴最后一刀
4. **B' 单发信封池化**(残余 RFC 修正后的真刀候选):Request/Response
   单发信封走 argv_pool 同款池。验收 = perfgate-median 集合角;
   不动针则如实弃(revert 是诚实答案)。

### R-C disk 轴
5. **compact 参数 sweep**:live_pct ∈ {30,50,70} × COMPACT_STEP_RECORDS
   ∈ {256,1024}:稳态 amp、压实追赶速度、**对 p99.9 的影响**(与 L5 互动
   正是"平衡"二字的落点)。
6. **AOF auto-rewrite 停顿 @ tiered+压缩负载**:rewrite 期间 PING
   p99.9 / 冷读 p99(rewrite 要读冷段 → 与 vlog 解码互动,从未测过)。
7. **realistic 语料稳态 amp**:templated 类语料长写 + 25% churn,
   稳态 vlog amp 与 dict 命中(同值语料的 0.01× 是天花板端,产品口径
   需要 realistic 稳态数)。

### R-D stable 轴
8. **soak 长跑**(先 1h,绿则 2h+):混合负载(KV+集合写+pubsub+tier
   churn+声明表写)@ 组合态;监控:frag 漂移曲线、used_memory 恒等式、
   PING gap 计数、vlog amp 漂移、reactor 心跳上界。**wedge 类 = 一票
   否决**。
9. **crashgate + repligate 复跑 @ 组合态**(既有门禁,换构建)。

### R-E 平衡点判定
10. 三轴代价表定稿 → **平衡点配置建议**(alloc 默认、compact 参数、
    rotate/预算默认)→ 喂 P1/P2;残余差距逐条标"收敛手段已知/未知"。

## 纪律
- 盒上一次一测(wait-for-quiet 已进脚本);长跑期间不并行其它测量。
- 每格数字 median-of-N 或带 spread;单跑不下结论。
- 参数 sweep 先问"这个默认是谁拍的"—— 拍的才 sweep,测过的不重测。
- 发现新缺陷:修小的、finding 大的,不在本轮开新弧。

---

## 结果回填(2026-08-08 收口;全账 = bench/FINDING-2026-08-07-balance-round-ra-rc.md)

**R-A 组合态基线(史上首同测)**:envelope 全 PASS 逐格与 OFF 同
(frag 1.32 vs 1.35 @4KiB)、冷读 90/210µs、median 角表 = 已知集合税
(−9~−15)其余全绿、混合风暴 **p99.9=3.2ms**。组合态在 disk/容量/
stable 轴零代价 —— P1 的取舍被精确定价为"只有集合角"。

**R-B**:信封池化死于实现前(85.7M op 普查:16B×1.96 + 32B×0.98
= 2.9 分配/op,信封类 0.5%);**真刀具名 = hash 小值内联**(消 ~2/2.9,
两构建同收益,店侧中爆炸半径,归 V4/P1 执行轮)。

**R-C**:rewrite 无罪(对照同尾);**1GB/s AOF firehose 100-250ms
p99.9 全属 AOF 追加路径**(no-aof 满压分层 ≤10ms —— 新子系统全部
无罪),V3 具名靶;compact 阈值 50 维持(epoch=0 之谜 = 轮转粒度:
8 shard 各一 active 文件永不密封;8MB rotate 下机制 6 秒收敛
amp 1.14×);realistic 语料压缩省盘 ~30%。

**R-D:三跑 soak,前两跑各挖一个工业级缺陷,全部当日修**:
① u16 钉死的 4GiB/类 abort 天花板(`7008e36f`)② maxmemory 每 shard
整份执行 = N× 上限(`d4075a6f`,init+tick 双点)③ evicted_keys 只在
# Memory 不在 Redis 惯例的 # Stats(`ed16456d`,加法双发)。第三跑
60 分钟满程:执行正确(2.2% 超额)、frag 收敛 2.2→1.67 无上漂、无
wedge;crashgate + repligate PASS(修完的组合构建;7101 上的 14 小时
孤儿 writer 验明正身后清除)。监控学费:子进程式采样自伤 gap 计数,
tailgate 机制化必须进程内探针。

**默认值定稿(喂 P1/P2)**:alloc ON(代价=且仅=集合角)/ compact 50
维持 / rotate 256MB 维持+文档+可配置化候选 / 工业配置双旋钮缺一不可。
r1-locality 现 **+42 笔**(tip `6908f92d`)。
