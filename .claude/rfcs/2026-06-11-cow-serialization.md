# RFC: COW 序列化 — O(n)-shallow freeze + 后台持久化(backlog E)

日期:2026-06-11 · 状态:已批准(user 选 ②,ceiling-first)· 分支:feature/cow-serialize

## 目标(ceiling-first,两条 ceiling 的优先序)

1. **不可侵犯**:稳态热路径零回归。perfgate 6/6 是硬闸;Str 内联(SmallBytes ≤22B 在 bucket 内)、Entry 48B / Value 32B 布局不动。
2. **本 RFC 的 ceiling**:持久化(SAVE / BGREWRITEAOF / tick auto-rewrite / embedded snapshot)的停摆从「整个序列化+磁盘写时长」(秒级/GB)降到 **collect-only 短窗**:O(n) 浅拷贝,~5-10ns/entry(10M keys ≈ 50-100ms),与磁盘速度、value 大小(集合类)解耦。后台线程完成真正的序列化与 I/O。

真 O(1) freeze 被否决:需要 persistent/HAMT 结构替换平铺 Swiss table,摧毁 ceiling 1(find_by_borrow 19.1% 热点,指针追逐 vs 平铺探测)。Redis fork-COW 的 page 级共享在纯 Rust 多线程进程里没有等价物(fork 不安全,seqlock 跨任意堆图不 sound)。

## 现状(2026-06-11 核实)

- keyspace = `KevyMap<SmallBytes, Entry>`:单 allocation 平铺 Swiss table,裸指针,**结构上不可跨线程共享**(后台线程不能与 shard 线程并发遍历)。
- `Entry` 48B = `Value`(32B)+ expire_ns + weight + lru_clock(const assert 锁死)。
- `Value::Str(SmallBytes)` 内联;集合类全部 `Box<HashData/ListData/SetData/ZSetData/StreamData>`(8B)。
- 阻塞现状:server SAVE/rewrite 在 shard 线程内联序列化(exec_op.rs / shard_tick.rs);embedded 后台 auto-rewrite 已有三段式(锁内 dump-to-buf + 放锁落盘),但锁窗 = 全量 RESP 序列化到内存 ∝ 数据量。

## 否决的备选

| 方案 | 否决原因 |
|---|---|
| persistent/HAMT 结构 | 杀热路径(ceiling 1) |
| entry 级 pre-image log + shard 线程增量序列化 | 序列化 cursor 被 mid-snapshot resize 打断,需 per-key serialized 追踪(O(n) 侧表)或 freeze growth;复杂且停顿只是被打散不是变小 |
| seqlock + 后台读 live 表 | 跨任意堆图的撕裂读在 Rust 里是 UB,不 sound |
| 三段式移植 server(原 ①) | 锁窗 ∝ 全量序列化字节数,不达本 RFC ceiling;被 ② 包含 |

## 选定设计:value 层 Arc-COW + collect-then-serialize

### 核心机制

1. **集合 variant `Box<T>` → `Arc<T>`**(std::sync::Arc,std 不算第三方依赖):尺寸同 8B,Value 仍 32B,const asserts 不变。读路径 deref 同一次指针追逐,零成本。
2. **写路径 `Arc::make_mut`**:唯一持有时(稳态)= 一次原子 load + 分支,~1-2ns,只落在集合写命令(HSET/LPUSH/…),Str 热路径(GET/SET)完全不经过。快照在飞期间首次写某共享集合 = 深 clone 该集合(COW 语义;等价 fork-COW 的页拷贝,粒度粗到整个集合 —— 已知取舍,文档化)。
3. **`Store::collect_snapshot() -> SnapshotView`**:遍历表,每 entry 收 `(key: SmallBytes::clone, ValueSnap, ttl_ms)`。Str/key 的 heap 模式 = 字节拷贝(见「大字符串」);集合 = `Arc::clone`(refcount bump)。SnapshotView: Send,后台线程随意慢慢序列化,point-in-time 一致(TTL 在 collect 时刻结算)。
4. **消费侧**:kevy-persist 增 view 版序列化入口(snapshot + rewrite);server 每 shard 一个 lazy 持久化 worker 线程(channel 投递 view;同时最多一个在飞,Redis 单 bgsave 同款);AOF rewrite 复用现有 `begin/finish_concurrent_rewrite` 的 tee 纪律(begin 改从 view 出);embedded:read 锁内 collect(短窗),放锁序列化。
5. **删除/驱逐与快照共存**:remove/evict 只是 drop 一个 strong ref,view 持有的数据自然存活到序列化完 —— 无需任何特殊处理;瞬时内存增长 = view 浅表(~56B/entry + heap str 拷贝)+ 快照窗内被改写集合的深 clone,文档化。

### 大字符串(heap SmallBytes)的两个子选项

- **(a) collect 时拷贝**(stage 1 采用):小 value 工况(北极星 bench、典型 Redis)collect 仍 O(n×56B);大字符串工况 collect ∝ 字符串总字节,但纯内存拷贝(~10GB/s)仍比「序列化+磁盘」快一个量级。不动 kevy-bytes(锁定布局的 stone)。
- **(b) SmallBytes heap 模式改 refcount**(buffer 前缀 8B 原子计数,24B 外形不变):collect 全程 bump。+8B/heap 值 RSS,clone/drop 变原子操作。**仅当 (a) 实测停顿不达标再立项**,需独立 microbench(clone/drop 热路径回归)。

## 阶段(每段独立可测,measure-gate)

1. **kevy-map:`Clone for KevyMap`**(K,V: Clone 深拷贝;alloc 同容量 + 逐 slot clone)+ KevySet。单测 + miri。
2. **kevy-store:Box→Arc + make_mut 扫(39 个 variant match 点 + 各类型模块内部 &mut 路径)+ `collect_snapshot`/`SnapshotView`**。测试:point-in-time 语义(collect 后改写/删除,view 不变)、COW 深 clone 触发、Value 32B assert、全量旧测试。
3. **kevy-persist:view 版 save_snapshot / rewrite plan**(与现有 byte-identical,用旧 loader 验证)。
4. **server 接线**:SAVE/BGSAVE/BGREWRITEAOF/tick → collect + 后台 worker;回复语义(SAVE 同步等?Redis SAVE 阻塞语义保留 = collect+等待落盘?→ SAVE 保持同步完成但停摆只剩 collect+等待,BGSAVE/auto 全后台);AOF tee。
5. **embedded 接线**:save_snapshot read-锁内 collect;reaper 三段式改 view(锁窗从 dump-to-buf 缩到 collect)。
6. **验证**:perfgate 6/6(必须);lx64 microbench:collect 停顿 @1M/10M keys、HSET make_mut 路径 A/B、快照在飞时写放大;快照一致性 + crash(tmp+rename 不变)测试。

## 实测(2026-06-11,M4 release build)

- collect 停顿:**1M string keys = 8.1-8.6ms(8.1-8.6 ns/entry)**,正中预估区间;10M ≈ 85ms。
- 集合解耦:10 × 100k-field hash(100 万嵌套字段)collect = **1 µs**(10 个 entry 的 Arc bump)—— 与集合大小完全无关 ✓。
- 测试常驻 tests_snapshot.rs(collect_pause_*,带宽松上限断言防深拷贝回归)。

## 实施中发现(不在本期修)

- **embedded save_snapshot 从不 truncate/重置 AOF**:snapshot + AOF 同开时重启 = snapshot 之上重放全量 AOF,非幂等命令(LPUSH 等)双重应用。既有 bug,与 COW 无关;server 侧同窗口已在 E-4 用「相邻提交」收窄。待独立立项(方案:embedded save 也走 tee-reset,或文档限定 snapshot 仅 aof=false 模式)。
- INFO `aof_rewrite_in_progress` 是常量 0 stub;后台化后真实状态有观测价值,需要 shard→dispatch 的 stats 通道(独立小项)。

## 风险

- make_mut 漏改某个 `&mut` 集合路径 → 编译期就挡(Arc 无 DerefMut)✓ 这是该设计的安全网:**借用检查器强制 COW 纪律**。
- 快照窗内大集合首写深 clone 尖刺:文档化;粒度优化(per-collection persistent 结构)留作后续独立 RFC,不进本期。
- 序列化输出必须与现 format 字节兼容(旧 loader 能读),用现有 round-trip 测试对拍。

## 参考

- 阻塞现状调查:本 session(exec_op.rs:212/273、shard_tick.rs:84、reaper.rs 三段式)
- [[feedback-autorun-ceiling-no-shortcut]] / [[feedback-mailrs-perf-methodology]](measure-first)
