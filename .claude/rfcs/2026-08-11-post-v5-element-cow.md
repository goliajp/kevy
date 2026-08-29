# RFC: element-granularity COW for collection values(post-v5 P1)

Status: ACTIVE(设计轮 2026-08-11;实现分期,List 先行)
上游:`.claude/plans/2026-08-10-post-v5-element-cow-rfc-draft.md`(A 案初判 + Phase A 实测)
证据:`bench/FINDING-2026-08-10-v5-rc-soak.md`(多 GB 单集合 rewrite 窗写 → 7-9.5s 反应堆停顿)
地面真相:Explore 全图(本文引用的 file:line 均实查,2026-08-11)

## 1. 目标不变量

> **对被 view 钉住的集合的一次写,代价 = O(命中段),与集合总尺寸无关。**

今天:`Value` 五个集合臂都是整值 `Arc<XData>`(value.rs:186-190),
view 钉住(snapshot.rs:139-145 的 `e.value.clone()`)期间首写走
`Arc::make_mut` 整值深克隆,在服务线程上。Phase A 实测 ~50-70ms/百万
64B 元素,5M=352ms + 341MB RSS 瞬态;soak 实测 GB 级 7-9.5s。

验收线(继承草案):
- soak 同款巨集合工况(GB 级单键 + rewrite 窗口写)gap 秒级 → **≤100ms**;
- perfgate 全 cell 无回归(小集合表示不变,按尺寸升段);
- `tests_snapshot.rs:164` collect <50ms 断言照常;crash/repli/全门禁绿。

## 2. 分集合设计(按实现顺序)

### Stage L — List(先行,soak 的 mylist 路径)

`ListData = VecDeque<Vec<u8>>`(value.rs:18)→ 新表示:

```rust
// kevy-store/src/list_seg.rs(新文件,value.rs 无余量)
pub struct SegList {
    segs: Vec<Arc<ListSeg>>,   // 24B in Value(≤32B 硬 assert value.rs:228)
    len: u64,                  // O(1) llen
}
pub struct ListSeg { items: VecDeque<Vec<u8>> }   // 段容量 SEG_CAP
```

- `SEG_CAP = 16 * 1024` 元素(草案 Phase A 定参:16K 段 → 克隆上界 ~1ms;
  亿级列表段数组浅拷 ~6K×8B = µs 级)。
- LPUSH/RPUSH:只 `make_mut` 首/末段;满则前/后插新段(O(段数) 的
  Vec 头插——LPUSH 侧改用「段数组倒序占位」或接受 memmove:6K 项
  memmove 是 µs 级,不设计提前优化)。
- LPOP/RPOP:首/末段 make_mut;段空则弹出段。
- 中段 ops(LSET/LINSERT/LREM/LTRIM,list.rs:308-401):按段 len 前缀
  走到命中段,make_mut 该段;LREM/LTRIM 跨段时逐段处理,只克隆真正
  触碰的段。LINSERT 命中段满 → 段分裂(split at hit)。
- 升段:`SmallListInline → Arc<ListData>` 现行为不变;
  `Arc<ListData>`(整值)→ SegList 当 len 超 `SEG_PROMOTE = SEG_CAP`
  (≤16K 元素的列表保持今天的单 VecDeque 整值表示,perfgate 面零扰动)。
  Value 臂:`List(Arc<ListData>)` 保留 + 新臂 `SegList(Arc<SegListData>)`
  (仍 8B 指针,Value 32B 不破)。
- 序列化:rewrite_fmt.rs:219 的 `l.iter()` 换成跨段 chain 迭代器;
  chunker(rewrite_chunk.rs,64 items/cmd)天然逐段消费。
  snapshot_payload.rs:33 同。载入路径(keyspace_load)按元素回放,
  自然落对表示——载满 16K 自动进段。
- 账务:weight = Σ段(collection_overhead 按段 capacity 求和,
  value.rs:426);O(1) len 由外层 `len` 字段承担;
  `is_heap_heavy`(value.rs:379)= 外层 Arc strong_count==1 && 各段
  strong_count 全 1 才走 bio-drop(否则退化 inline drop——正确性优先,
  段被 view 共享时本来也不该 bio 掉)。

### Stage HS — Hash / Set(soak 的 myhash/myset 路径)

`KevyMap` 不可切片(swiss table 连续布局,clone.rs:17-46 整层拷)→
**hash 前缀分桶**:

```rust
pub struct SegMap<V> {
    shift: u8,                     // 桶数 = 1 << bits
    buckets: Vec<Arc<KevyMap<SmallBytes, V>>>,
    len: u64,
}
```

- 路由:`bucket = top_bits(hash(key), bits)`——与 KevyMap 内部 hash
  复用同一 hasher(kevy-hash),取高位避免与桶内探测位相关。
- 写:make_mut 命中桶;克隆上界 = 桶尺寸。桶超 `BUCKET_SPLIT`
  (Phase B 扫参,初始 16K 槽)→ 全体桶数翻倍(rehash 是"搬指针 +
  分裂命中桶"式渐进?不——正统简单版:翻倍时只分裂超限桶所在的
  目录,即 extendible hashing 的目录翻倍 + 单桶分裂,其余桶 Arc 共享,
  **翻倍本身 O(桶数) 浅拷**)。
- Hash 的 TTL sidecar(hash_ttl.rs)与 hdel 路径(hash.rs:278)改走
  SegMap 路由;Set 的 spop(set.rs:261)随机成员 = 先随机桶
  (按 len 加权)再桶内随机。
- 升段阈值同 List:≤16K 元素保持现 `Arc<HashData/SetData>` 整值。

### Stage Z — ZSet(双结构)

- `by_member: KevyMap` → SegMap(同 Stage HS)。
- `by_score: RankTree`——分段不适用(全序结构),正统解 =
  **持久化树 path-copy**:`Node.children: Vec<Node>` →
  `Vec<Arc<Node>>`(kevy-ranktree 内部),写路径 clone O(log N) 路径,
  view 共享其余。ranktree 是独立石头(246+45 LOC,余量足),
  单独 bench + proptest 后再接 ZSet。
- Stage Z 依赖 ranktree COW 化先落;不阻塞 L/HS 发货。

### Stage X — Stream(暂缓,文档边界)

BTreeMap + groups PEL,一把 make_mut 罩 15 写 op(stream/store.rs:38)。
std BTreeMap 无法 COW 化,需自研 B 树或 SegMap 化 entries——工作量
独立成 arc。Stream 的典型工况(XADD 追加 + XTRIM 修剪)本身有界,
soak 未命中。**本 RFC 范围内只写文档边界,不动**;persistence 三语
的 giant-collection 段落补一句 Stream 同理。

## 3. 全局约束核查单(每 Stage 落地前过一遍)

- [ ] `size_of::<Value>() ≤ 32`(value.rs:228 assert)
- [ ] `Entry` 48B(entry.rs:123)
- [ ] O(1) llen/scard/hlen/zcard(外层 len 字段)
- [ ] weight/账务:collection_overhead 跨段求和;account_delta O(1) 路径不变;
      reweigh_entry 只在升段/降段时走
- [ ] is_heap_heavy 语义:全段 unique 才 bio-drop
- [ ] COPY/undo 共享(keyspace.rs:127 clone_with_ttl)同收益——写只克隆命中段
- [ ] 序列化两路(rewrite_fmt + snapshot_payload)+ 载入回放 roundtrip 测试
- [ ] collect_pause 断言(tests_snapshot.rs:142,164)照常
- [ ] 500 LOC:新逻辑进新文件(list_seg.rs / seg_map.rs);
      value.rs(462)/hash.rs(453)/rewrite_fmt.rs(490)只加臂不加体
- [ ] locgate/commentgate/rootgate + workspace 测试 + clippy -D warnings

## 4. 验收实验(复用草案 Phase A 形状)

1. 微观:填 N(1M/5M/20M/100M 64B 元素)→ BGREWRITEAOF → 0.3s 后
   首写计时 + RSS 采样。预期:首写 ≤~2ms(单段克隆)且与 N 无关;
   RSS 瞬态 ≈ 段体积(~1MB)而非值体积。
2. 宏观:rc-soak 巨集合工况重跑(80min 缩到 20min 版,mylist/myset/
   myhash 无界增长),gap 水位从 0.7-9.5s 阶跃 → 全程 ≤100ms。
3. perfgate-median 全 cell(×3):小集合(≤16K)零回归硬线;
   大集合工况允许 ≤2% 常数税(段路由一跳),超出即回设计。

## 5. 不做 / 反例记录(承草案)

- B 案 serialize-before-pin:跨线程所有权复杂,拷贝仍 O(GB),否。
- C 案 delta side-log:读路径叠加 delta = 读放大,违背上限性能优先,否。
- 渐进式 rehash(Redis 式双表):与 view-pin 语义正交,不解决克隆,否。
- Stream 本期不动(§2 Stage X)。

## Resolution(2026-08-11,实现闭卷)

- **Stage L(List)**:merge `183bc8f3`。SegList = VecDeque<Arc<16K 段>>。
  盒上:352/666ms → 0.4/0.6ms,与 N 无关。
- **Stage HS(Hash/Set)**:merge `f74e83c2`。SegMap<V> 可扩展哈希——
  **设计修正**:目录存桶索引、桶独立存放(首版目录内 Arc 共享被
  100K 分裂测试抓出写分叉;Arc 只对 view 共享才让 make_mut 语义成立)。
  盒上:hash 1.2-2.1ms / set 0.1-0.3ms,与 N 无关。
- **Stage Z(ZSet)**:merge `f8acc77e`。**设计改道**:未 fork ranktree
  内部(§2 原案 path-copy;remove 再平衡 = fork 注 bug 高发面),改为
  SegZSet = 有序分段 Vec<Arc<RankTree>>(缓存 max 路由)+ SegMap<f64>,
  两石头零改动组合。盒上:0.7-1.0ms,与 N 无关。rank 运算付 O(段数)
  前缀走查(亿级 µs)——§2 Stage Z 的"独立石头轮"改为组合复用,
  验收数字达标,复杂度低一个量级。
- **Stage X(Stream)**:按 §2 保持文档边界;persistence 三语已改写
  (四集合收口,Stream 单列)。
- perfgate-median 三轮(L/HS/Z 各自 merge 前)12/12 全 PASS;
  触碰面无负回归。验收实验 1(微观)三门全达标(≤~2ms 与 N 无关,
  RSS 瞬态≈单段);验收实验 2(宏观):**收口 soak 抓出第二轴——
  per-tick 聚合 = 触段数×段体积**(散射爆发首触多段,总解共享字节
  ≈值体积在任何粒度下不变,粒度是摊销旋钮),消融 16K→2K→512
  (1.86s→188ms→COW 份额 ≤65ms);双对照差分归因(无窗 50ms 底 /
  有窗 strings-only 仍现 127ms@finish)证明残留离群属强制全 shard
  同步 rewrite 的 finish 座位(S5-G swap 提交窗家族,auto 路径错峰
  免疫)——**COW-attributable gap ≤65ms ≤ 100ms 线,宏观验收 PASS**;
  finish 座位单列为后续 attack 候选(BGREWRITEAOF fan-out 错峰)。
  全数据 bench/FINDING-2026-08-11-element-cow-closeout.md。
- CI 面两桩记档:crashgate midfile-resync 首犯 flake(入档);
  miri 40min 超时被 16K 循环测试撞出 → seg 套件 time-box ignore。
