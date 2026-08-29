# 透明分层存储（`[tiering]` / `with_tier_budget*`）

分层存储给 kevy 一个 **RAM 预算**：当键空间涨过预算，最冷的那些值会下沉到磁盘上一份按启动周期存在的 value log，并在被访问时换回内存。键和元数据始终留在 RAM 里——所以 **RAM 决定你能放多少个键，磁盘决定你能放多少数据**——而每条命令在冷键上保持精确不变的语义。没有第二套 API，没有“冷命名空间”，也没有需要处理的 cache-miss 错误：冷键就是一个普通的键，只是第一次物化它的值时要付一次定位磁盘读。

```toml
# kevy.toml
[tiering]
budget = "auto"        # or "70%" or "4gb"
```

```rust
let store = Store::open(Config::default()
    .with_dir("/var/lib/kevy")
    .with_tier_budget(3 << 30))?;   // 3 GB RAM budget, embedded
```

分层存储**默认关闭**。没有 `[tiering]` 段（也没有调用 `with_tier_budget*`）时，引擎的每条路径与未分层的构建逐字节相同——这是一条有门禁把守的主张，不是一句愿望。

## 它是什么——以及它不是什么

- **它是一个 RAM 预算。**越过预算 demote 水位线的值会移入 value log（`<data>/tier/`），在条目原有的 value 槽位里留下一个 stub——不占额外的堆。值的 RAM 被完整回收；键、它的 TTL、类型和 LRU 历史都留在内存里。
- **它不是持久化。**AOF 仍然是唯一的持久事实，而分层存储**从构造上就不增加任何新的崩溃安全面**：value log 是一块**按启动周期存在的下沉区**——store 打开时删除、重放期间重建——所以它从来不属于崩溃可能丢失的东西。你的崩溃保证就是 [persistence.md](persistence.md) 的保证；分层持久化测试套件还额外钉死了分层与持久化交汇的两条路径（在大部分数据已冷的 store 上做 rewrite / 快照，以及数据超过预算时的启动）。
- **它不是淘汰（eviction）。**被 demote 的键仍然存在——`EXISTS` 回答 1，`SCAN` 返回它，`TTL` 继续倒数，`GET` 照常回答。八种 `maxmemory` 删除式淘汰策略保持精确不变的语义；如果你同时设了 `maxmemory`，它仍然是 tier 预算之上那道硬性的删除式淘汰兜底。demote **不发出任何** keyspace 事件（它不是写入也不是淘汰——客户端把 `evicted` 理解为键被删除）；它计入独立的 `demotions_total` 计量，从不混进 `evicted_keys`。
- **索引地板不可下沉——挑预算之前先知道它。**索引、视图和保存的 `VALUES` 列常驻内存（它们正是让冷行查得便宜的东西）。对索引很重的 store，可触达的 RAM 节省被那块地板**之外**的部分所限：预算低于地板会把 demote 目标压到 0——tier 把能下沉的都下沉一遍之后就再也无事可做——在 `INFO` 里表现为 `tier_effective_target:0`，且新的索引声明会被按名拒绝。预算要从下面预算模型里的地板公式来定，不要从原始数据量来定。

## 启用它

服务端——任何一个配置面都行，同一个键名：

```toml
[tiering]
budget = "auto"              # 0.70 x the detected memory bound
# budget = "70%"             # percent of the detected bound
# budget = "4gb"             # absolute
# spill_dir = "/fast/nvme"   # optional; default <data_dir>/tier/
```

```console
kevy --tiering-budget 4gb          # CLI
KEVY_TIER_BUDGET=4gb kevy          # env
kevy-cli CONFIG SET tiering-budget 6gb   # live: budget CHANGES only
```

`CONFIG SET tiering-budget` 在下一个 shard tick 重新解析（沿用 `maxmemory` 的先例）。打开或关闭分层存储、移动下沉目录需要重启——value log 的生命周期以启动为界。

嵌入式——在 `tier` cargo feature 之后（默认集合里有；依赖 `persist`，因为下沉区需要数据目录）：

```rust
Config::default().with_tier_budget(bytes)      // absolute
Config::default().with_tier_budget_auto()      // 0.70 x detected bound
Config::default().with_tier_budget_percent(50) // percent of the bound
```

纯内存 store（`mem://`，或没有数据目录的 `Config`）以及 wasm 构建会在 open 时用一个具名错误**拒绝**分层配置——没有磁盘可以下沉，而一个被静默忽略的预算，等于一个披着启动成功外衣的错误答案。

`auto` = 检测到的内存上界 × 0.70：Linux 上取 `min(cgroup v2 memory.max, /proc/meminfo MemAvailable)`；macOS 上取 `hw.memsize`（`kevy-sys` 里手写绑定的 `sysctl`，全 workspace 唯一被允许的 OS 边界）。上界会**在 shard tick 上重新探测**，所以容器的内存限额被调整后会实时跟上。检测不到任何上界的主机会在启动时按名拒绝 `auto` / 百分比——请改用绝对预算。

## 预算模型

整个进程一份预算，均分给各个 shard。每个 shard 朝一条统一的水位线 demote：

```
demote target = budget·19/20 − index_reserved_bytes − stub_bytes
```

- **索引和视图是优先保留的固定层**——它们从不下沉（它们正是让冷行可以被便宜地找到的访问路径），所以它们的字节从预算顶上先扣掉。如果光是索引的下限就超过预算，`IDX.CREATE` / `TABLE.DECLARE` 会用具名错误**拒绝**，而不是收下一个预算装不下的索引。
- **stub 字节同样要扣**：已冷键的 stub 是预算必须背着的 RAM，所以冷层越大，水位线越紧。当固定下限超过 19/20 线时，有效目标饱和到 0——在 `INFO` 里可见，不被隐藏。
- 19/20 这个系数是滞回带：demote 在线上方开始、线下方停止，store 不会在线附近来回震荡。
- 下沉是**限额的**：每次 demote 调用最多 32 条记录，剩余在 shard tick 上续跑——单条 `SET` 永远不会引发一场无界的同步下沉风暴。

### 每个键的 RAM：热与冷

```
hot  key ≈ today's cost (entry + key + value)
cold key ≈ 96 B (entry overhead) + key heap bytes     # value fully reclaimed
```

冷键公式以 ±20 % 的容差对照实测 RSS 由门禁把守（`bench/memgate.sh`）。有两条值得据此做规划的推论，都来自这套容量模型：

- **约 64 B 的值永远不可能有利可图地分层**——stub 差不多和值一样大。低于 64 字节下沉门槛的值从不下沉。`data:RAM` 比随值大小线性增长；每条容量门禁都写明自己的值大小，原因正在于此。
- **已实测（2026-08-05）。** 比例曲线不再只是模型。同一预算、同一键长，只变值大小：

  | 值大小 | 实测 data:RAM |
  |---:|---:|
  | 256 B | **2.65×** |
  | 1 KiB | **10.43×** |
  | 4 KiB | **39.2×**（全尺度：2 GB 预算扛 80 GB 数据） |

  地板是每条目约 96 B，在测过的每个尺度上都是平的（键长 9 B 时；键长 48 B 时约 143 B），于是上限可以预测：**上限 data:RAM ≈ 值大小 /（96 B + key heap）**，对上面三行分别预测 2.67× / 10.7× / 42.7×。这条公式的交互版在 <https://kevy.golia.jp/zh/capacity/>。

**那 96 B 究竟是什么，决定了杠杆在哪里。** 它是**键空间条目**（`ENTRY_OVERHEAD`：内联的键单元加上那个 `Entry`），是**每一个**键无论有没有被分层都要付的——冷 stub 本身 24 B 内联、不占堆。分层能把值还回来，却永远还不回那个给值命名的键，所以**没有任何分层旋钮能移动这个数字**；能移动它的只有 store 的条目布局，而改动它意味着改动每一种工况下每一个键的代价。

  **256 B 时预算不是紧，而是根本守不住**：20 万条就越过 16 MB 预算，80 万条时用到 77 MB——因为降级一个 256 B 值省下的还不够它留下的 stub。窄记录要手算着定预算。
- **算例（按模型推算，不是实测）**：10 M 行 × 约 1 KiB（约 10 GB 数据）加 2 个二级索引和存储的 VALUES 列，装进 **3 GB** 预算：stub 下限 10 M × 约 108 B ≈ 1.1 GB，索引下限 10 M ×（68 + 68 + 约 30 VALUES 字节）≈ 1.7 GB，合计 ≈ 2.8 GB ≤ 3 GB。值为 4 KiB 时，比例门禁是 ≥ 10× `data:RAM`（5 M × 4 KiB = 20 GB 对 2 GB 预算；stub 下限 ≈ 540 MB）。窄行由每键固定成本主导——在相信任何预算之前，先把 stub 下限和索引下限算一遍，这正是公式放在前面的原因。

## 冷键上的语义

每条命令都是透明的。stub 携带值的**类型标签**，所以整个元数据面**零磁盘读**作答：

- `SCAN` / `KEYS` / `RANDOMKEY` / `DBSIZE` 看得见冷键——键表只有一张，SCAN 的保证不变。`SCAN … TYPE t` 直接按标签过滤，不碰磁盘。
- `TYPE`、`EXISTS`、`TTL` / `EXPIRE` 族、`RENAME`、`DEL`、`PERSIST` 从不读值。对冷键的 `DEL` / 覆写会给 value log 记入 dead-bytes 并释放 stub——不发生读。
- **一次 `WRONGTYPE` 拒绝永远不用付磁盘读**：类型检查在任何 I/O 之前就从 stub 上解决（对冷 string 的 `LPUSH` 被拒绝的代价与热键相同）。
- `SET` 族的 `NX` / `XX` 从 stub 读存在性；`FLUSHALL` 清空键表并重置 value log。
- 冷 hash 的**字段级 TTL 留在 RAM 里**，并且能撑过一轮 demote / promote 往返；在冷期间过期的字段会在 promote 时被清除。
- `WATCH` 不会被 demote 或 promote 触发——层间移动不是写入。

这些不是叙事性的说法：**透明性测试套件**对一个分层 store 和一个未分层 store 重放同一操作序列，并断言语义命令的回复逐字节相同——按契约无序的回复（`HGETALL`、`KEYS`、`SCAN`、`RANDOMKEY`，它们的元素顺序就是 map 顺序）改为形状比较——（内存报告类命令做形状比较——它们理应不同），demote 时机由确定性强制注入，从不依赖时序。

### 换回（promotion）策略

- 冷值在**第二次物化访问时换回**。第一次冷读直接把字节从 log 里端出去、不装回内存（盖一个试用期标记）；第二次才装回。对一个归档键的一次偶然读取，不会搅动热层。
- **批量路径从不 promote**：hydration（`FIELDS`、`VIEW.HYDRATE`）、索引回填、`PREFIX.DIGEST`、scope 迁移、导出、快照 / AOF rewrite 序列化，都通过一条不 promote 的 peek 路径读冷值。在一张全冷的表上做 `IDX.CREATE` 回填，每行读一条记录、一条都不装回。
- 零拷贝共享通道（C ABI 上的 `kevy_get_shared`）每次调用读一次冷值、从不 promote——共享通道的读者要一直付这笔读，直到某条正常访问路径把键换回。
- 对冷键的 `MEMORY USAGE` 报告的是 **stub** 的 RAM 占用（这个键现在真实花掉你多少）；原始值的大小随 stub 一起保存，promote 时重新入账。

## v1 里什么会下沉

字符串（高于 64 字节门槛的）和 **hash**——一个 hash 作为一条携带全部字段的记录下沉，这正是一行冷表数据只花一次读、而不是每字段一次读的原因。list、set、sorted set 和 stream 在 v1 里**留在热层**——这是一条具名的限制（集合类型的下沉在 post-v4 清单上），不是错误：它们只是不作为 demote 候选，预算算术把它们当作永久的热字节。

## `INFO`——`# Tiering` 段

仅在分层存储启用时出现（未分层的服务端 `INFO` 与完全没有分层能力的构建逐字节相同）；服务端与嵌入式 listener 上完全一致。

| 字段 | 含义 |
|---|---|
| `tiering_enabled` | `1`（关闭时整段不出现） |
| `tier_budget_bytes` | 解析后的预算（auto / 百分比 → 字节，实时） |
| `tier_effective_target` | `budget·19/20 − reserved − stubs`，饱和到 0——0 表示光固定下限就超过了水位线 |
| `cold_keys` | 当前处于 demote 状态的键数 |
| `cold_bytes` | 这些值 demote 前的原始字节数 |
| `stub_bytes` | 冷键 stub 占用的 RAM |
| `index_reserved_bytes` | 从水位线里扣掉的索引 / 视图下限 |
| `vlog_size_bytes` | value log 在磁盘上的大小 |
| `vlog_live_bytes` | 仍被引用的字节（其余可压实） |
| `vlog_files` | value log 段文件数 |
| `vlog_epoch` | 压实纪元（退役文件计数） |
| `demotions_total` | 启动以来下沉的值数 |
| `promotions_total` | 启动以来换回的值数 |
| `peek_preads_total` | 不 promote 的冷读次数（每个冷**行**一次） |
| `batch_submissions_total` | 批量冷读提交次数（hydration 页） |

`vlog_size_bytes / cold_bytes` 是空间放大比，验收门禁把它压在 ≤ 2.0×；`peek_preads_total` 是你验证一页 hydration 每行只付一次读、而不是每字段一次的办法。

## 性能预期

照实说，并标明每个数字是实测还是待测：

- **热路径：结构不变。**热值读写路径只多出一个永远不会命中的 match 分支；分层编译进来但关闭时，perfgate 的 12 项指标按既有容差把关；分层打开且工作集全热时，新的 `tiered_hotset_*` 行以同样方式把关。**基线待专用基准机记录**——门禁行已经存在，在那之前显式跳过并提示。
- **冷点读 = 一次定位读**加一次 CRC 校验和一次解码。`kevy-vlog` 微基准测得 `read_at` 在各记录大小下为 0.64–14 µs（开发机 NVMe）。门禁要压的端到端 SLA——标量 p99 嵌入式 ≤ 100 µs / 服务端 ≤ 300 µs，整行 hash 物化 ≤ 200 µs / ≤ 500 µs——**服务端已实测**：envelope 运行下标量 p99 落在 79–171 µs、整行 hash 145 µs，数据集从 20 GB 涨到 120 GB（6 倍）延迟基本持平。嵌入式那两个仍是目标——envelope 驱动的是服务端。
- **嵌入式在冷读期间持有 shard 锁。**进程内没有 reactor 可以把读转交出去：一次冷物化在 shard 写锁下做 pread（1-shard 默认 = 整个 store），期间该 shard 的读者会被停住——NVMe 上是 µs 级，云块存储上可能到 ms 级，值越大按比例越久。嵌入式配置默认把可下沉的最大值封在 **256 KiB**（`max_spill_value`；`with_max_spill_value(bytes)`，0 = 不限），把这个窗口约束住——超限的值就留在热层。服务端不设上限（thread-per-core 的 shard 不共享锁）。彻底消掉这个窗口的 drop-lock / pread / relock 方案已有设计，明确排在 post-v4。
- **批量 hydration：每行一次读。**一页 `FIELDS` hydration 或对冷行的 `VIEW.HYDRATE` 会按 log 位置合并读请求、作为一批提交（io_uring：副环上的链式读；poller / 嵌入式：有序定位读循环）。一次读解码一行**全部**被请求的字段——计数器断言 `preads == cold rows`，从不是 `rows × fields`。
- **index-only 查询零行触达**——FILTER / SORT / COUNT 从常驻 RAM 的索引列作答，所以在一张全冷的表上它们做零次磁盘读（计数器断言）。这正是在分层表上声明 `VALUES` 列的理由：见 [tables.md](tables.md)。
- **下沉从不无界地停住 reactor**：demote 每次调用限额、tick 上续跑；停顿钳制（下沉引发的 p99 ≤ 1 ms）是一条独立门禁行，同样待基准机实测。
- **空闲成本收敛到近乎为零**（4.1）。两个机制，缺一不可——"幂等不等于收敛"，这是一位消费者实测分层打开后空闲 CPU 高出 300–500 倍、进而把功能关掉换来的教训：
  - 每 tick 的索引 / 视图地板馈送由**世代缓存**供给：空闲的 store 什么都不重算；写压之下，每个 segment 统计也都是 O(1) 的运行计数器，而不再走查索引结构。
  - **demote 采样器指数退避**——当一个超目标却搬不动任何东西的 tick 出现时（包括此前保证每 tick 全量采样走查的 `effective_target = 0` 状态）。任何一次 demote 都重置退避；写路径自身永远立即采样，所以新出现的可下沉值不会等完窗口（上限 ~6 秒）。实测：空闲 30 秒 CPU 为关闭时的 **1.6 倍**（取代此前的 300–500 倍）。

## 运维须知

- value log 位于 `<data>/tier/`（或 `[tiering] spill_dir`），**每次 open 时删除**，随重放越过水位线而重建。不要备份它；不要让两个 store 指向同一个下沉目录。
- 段文件按 256 MiB 轮转；已封口的段在存活比低于 50 % 时压实（空间放大 ≤ 存活冷字节的 2.0×，有门禁）。压实尊重 pin——进行中的快照或 AOF rewrite 会让它的段文件保持可读，直到它结束。
- 每条记录携带 CRC32C；下沉区里的位腐坏在读取时被拒绝，而不是被端出去。
- **数据集 > 预算的启动能工作**：重放一边走一边检查水位线、就地下沉，而不是在分层还没运转之前就 OOM。同样的就地 demote 也作用于 reshard 与副本快照装载。2026-08-05 实测——2.3 GB 的 AOF 对 64 MB 预算（36 倍）干净重放，30 万行全在，`used_memory` 稳定在 35 MB，离上限很远。

  **RSS 是另一回事，本页此前把它说过头了。** 它声称启动全程 RSS ≤ 预算 × 1.05 并称其"有门禁"，两句都不成立：那次重放实测 RSS 峰值 **137 MB，是预算的 2.15 倍**——与容量扫描在稳态测到的是同一份分配器开销；而 `tiergate` 的 L11 行**根本还没有测量体**（显示 PENDING）。启动全程成立的是**逻辑**边界，那才是分层记的账；**按 RSS 配机器，别按预算配**。
- 在大部分数据已冷的 store 上做快照 / `BGREWRITEAOF` / 复制全量同步，会从被 pin 住的 log 流式读出冷值而**不 promote 任何东西**——额外 RAM 峰值是一个值，且 rewrite 不丢任何冷值。**2026-08-05 实测，不是门禁**：60 000 个键其中 54 912 已冷，`BGREWRITEAOF` 后重启，60 000 个全部回来，跨区间抽检的每个键长度都对，AOF 重放干净。`tiergate` 对应的 L10 行仍是 `PENDING`，所以这条靠的是那次实测，不是 CI。
- **持续保持 `used_memory` ≤ 预算 × 1.05** 是一条独立门禁行（`tiergate` L8），包括 auto 探测在 cgroup 容器内和裸机上都回答正确。它压的是**逻辑**边界——门禁把 RSS 报在旁边，而不是钳住它。2026-08-05 实测：`used_memory` 253 MB 对上限 281 MB，同时报告 RSS 488 MB（预算的 1.93 倍）。把这行读成 RSS 保证，正是本页上面两条曾经犯的错。

## 门禁状态（诚实账本）

上文所有机制性主张都由本树中运行的测试和门禁覆盖（透明性套件、分层持久化套件、`bench/memgate.sh`、`bench/tiergate.sh`）。**envelope 数字**由专用基准机上的 `bench/capacity-envelope.sh` 实测：冷读 p99、写入翻涌下的 vlog 空间放大、容量比，本页与 `bench/FINDING-2026-08-05-capacity-ceiling-sweep.md` 都有数。

而 `bench/tiergate.sh` **在一份全新检出里仍然把那几行显示为待测**——这是设计而非自相矛盾：门禁消费的是基准机产出的结果文件，没拿到结果文件的树就是什么都没验证过。跑完 envelope、把 `bench/.capacity-envelope-results` 带回来、再 `TIERGATE_RUN_ENVELOPE=1 bash bench/tiergate.sh`，那几行才凭证据翻绿，而不是凭这段话。只跑一半会写进它自己的结果文件，理由相同：**绝不能把缺行的结果文件递给门禁**。

## 参见

- [tables.md](tables.md)——与分层一起设计的 TABLE.* 层：索引热、行冷、index-only 查询零行触达。
- [persistence.md](persistence.md)——被刻意保持不动的持久化契约。
- [tuning.md](tuning.md)——与 tier 预算共存的内存旋钮（`maxmemory` 及其同伴）。
- [`bench/tiergate.sh`](../../bench/tiergate.sh) / [`bench/capacity-envelope.sh`](../../bench/capacity-envelope.sh)——本页引用或悬置的每个数字背后的验收门禁与 envelope 运行器。
