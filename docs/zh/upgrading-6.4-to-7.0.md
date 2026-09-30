# 从 6.4 升级到 7.0

简单说：**走协议的客户端不用改代码，数据目录原样打开。** 换二进制之前，有四件事要看一眼：

- 同样的数据，`used_memory` 读数更高，字符串和哈希混合的数据大约是原来的 1.5 倍，因为现在算的是分配器实际占着的内存。`maxmemory` 如果设得贴近 6.4 报的数字，会更早开始淘汰（[§4](#4-used_memory-对同样的数据读数更高)）。
- 有副本的话，主节点还跑 6.4 时副本上没有索引、视图和表，主节点跑上 7.0 后副本会拿到主节点的 catalog（[§2](#2-复制部署先升级主节点)）。
- 退回 6.4 会丢掉索引、视图和表的 catalog，除非留着升级前的附属文件；在 7.0 下被杀掉的嵌入式存储，还得先用 7.0 干净地打开并关闭一次；最后一步是在快照之后重写日志的目录，还要在 7.0 下再保存一次，否则 6.4 会把一部分写入应用两次（[§1](#1-退回-64目录里可能有什么)）。
- Rust 调用方要改代码，要改的地方编译器会逐个指出（[§13](#13-rust-api)）。

升主版本是因为后三类改动：整个 workspace 的公开 Rust API 都按 Rust API Guidelines 整理过，大多数 crate 的签名都变了；`kevy-config` 的结构体多了公开字段，结构体字面量会编译不过（§10）；Go 模块改成了 `github.com/goliajp/kevy-go/v7`（§14）；7.0 写过的目录里有 6.4 不认识的文件和日志帧（§1）。

7.0.0 主要做了四件事：嵌入式存储在进程被杀时保住每一次已经返回的写入，打开和关闭也更快；全局索引，按值分布到各个 shard；索引的每一行只占原来的四分之一或更少；加密链路，包括节点之间的链路和第二个客户端端口。这些都是不配置就不开启（[encrypted-links.md](encrypted-links.md)）。它还修了一批会丢数据或改数据的缺陷，有的从 1.x 就存在，见 [§16](#16-修掉的丢数据或改数据的缺陷)。

```toml
kevy-embedded = "7.0.0"
```

## 一句话对照表：要做什么

| 如果你…… | 会变什么 | § |
|---|---|---|
| 跑服务器，或者走协议访问 kevy | 换二进制；检查 `maxmemory` | 4 |
| 设了 `maxmemory`，或者跑分层存储的服务器 | 同样的数据，`used_memory` 读数高约 1.5 倍；分层存储的服务器在进程占用超过预算时，还会拒绝让数据变大的写入 | 4 |
| 有副本 | 先升哪边都行；主节点跑上 7.0 后副本拿到它的 catalog，副本自己声明的索引会丢掉 | 2 |
| 向副本发 `BLPOP`、`BRPOP`、`RENAME`、`RENAMENX` 或 catalog 命令 | 以 `READONLY` 拒绝 | 3 |
| 可能退回 6.4 | 用 `SHUTDOWN SAVE` 停掉 7.0（嵌入式：先 `save_snapshot` 再关闭）；留好升级前的 catalog 附属文件 | 1 |
| 给索引设了 `MAXMEM`，或者分层存储的容量贴着索引下限 | 索引大小的读数变了：大索引更小，每个 shard 至少约 1.8 KB | 5 |
| 按位置解析 `IDX.LIST` 或 `IDX.DESCRIBE` | 各多了一对 `partitioning` | 6 |
| 读变更流或 AOF | 嵌入式的一次 `MSET` 变成每个 shard 一帧；流的写入按重放需要的形式记录 | 7 |
| 不小心在同一端口起了两个服务器 | 第二个现在会拒绝启动 | 8 |
| 用 io_uring reactor | 接收缓冲区每个 shard 16 MiB，原来是 64；处理完转发来的工作后，shard 会轮询 200 µs | 9 |
| 用结构体字面量构造 `kevy_config` 的结构体 | 写上新字段，或者用 `..Default::default()` | 10 |
| 实现 `kevy_rt::Commands` | 多了几个方法，都有默认实现 | 11 |
| 脚本里以裸词调用 `kevy-cli doctor`、`export`、`sql compile` 等 | 把工具写到 `--kevy` 后面 | 12 |
| 把 kevy 的 crate 当 Rust 库用 | 大多数签名变了，编译器会逐处指出 | 13 |
| 用 Go 模块，或者匹配绑定层只读错误的文本 | import `/v7`；错误文本末尾补上了句点 | 14 |
| 用 `XAUTOCLAIM` 的游标、`5-` 这样的 id，或者对快要过期的键发 `EXPIRE` | 行为改成和 Redis 一致 | 15 |
| 想让一次索引读取只碰更少的 shard | 把它声明成全局索引 | [索引](indexes.md#全局索引partition-global) |

---

## 哪些东西原样沿用

对照的是 6.4.0 发布版二进制（从 `v6.4.0` tag 构建）和 7.0.0，各两个 shard，在 Linux 上用 io_uring reactor 实测；嵌入式存储另外在 macOS 上也测过。数据集里有字符串、一个键级 TTL、一个带字段 TTL 的哈希、一个列表、一个集合、一个有序集合、一个带消费组的流（一个消费者由读取创建，一个由 `XGROUP CREATECONSUMER` 创建）、两个索引、一张表和一个物化视图，中途做过一次 `BGSAVE`，所以有些写入只在日志里。

| 场景 | 键、TTL、字段 TTL、流 | 索引、视图、表 |
|---|---|---|
| 6.4.0 的目录用 7.0 打开 | 全在 | 全在；附属文件的内容搬进日志，文件删掉 |
| 7.0 的目录用 6.4.0 打开，只有日志 | 全在，只由 `XGROUP CREATECONSUMER` 创建的消费者除外 | 没有 |
| 7.0 的目录用 6.4.0 打开，有快照和日志 | 全在 | 没有 |
| 6.4.0 写过之后再用 7.0 打开 | 全在，包括 6.4.0 写的 | 回来了，除非 6.4.0 重写过日志（§1）|
| 6.4.0 主节点 → 7.0 副本 | 全量同步和实时流都能收敛 | 没有；见 §2 |
| 7.0 主节点 → 6.4.0 副本 | 全量同步和实时流都能收敛；由 `XGROUP CREATECONSUMER` 创建的消费者要到下一次全量同步才到 | 没有 |

6.4.0 读到新的帧，日志里不会报任何东西。**备份就是复制**：复制出来的目录和原目录提供的数据一样，进程被杀之后复制的也一样（§1）。

## 1. 退回 6.4：目录里可能有什么

### 崩溃或被杀之后

以前进程被杀时，还在用户态缓冲区里等着的写入会丢。7.0 让每次追加在返回的那一刻就落在内核持有的内存里：

- **在 Apple 平台上**，映射的是 AOF 本身，末尾预分配一段（4 MiB 起，翻倍到 64 MiB 为止），追加就是往里复制。存储打开期间文件比其中的记录长，多出来的部分全是零；干净关闭时会截掉。被杀的进程会留下这些零，下次打开时清掉。
- **在其他平台上**，追加先进一个暂存环，也就是一个小的映射文件 `aof-<i>.aof.stage`（默认 4 MiB），每个 tick 排进 AOF。下次打开时会重放被杀的进程留在里面的内容，在原目录里是这样，在被杀之后复制出来的目录（备份就是这么做的）里也一样。

6.4 两样都不认识。用一个嵌入式写入进程实测，在连续写入的中途用 `SIGKILL` 杀掉：

| 被杀后原样的目录 | 6.4.0 直接打开 | 7.0 打开并关闭一次，再用 6.4.0 打开 |
|---|---|---|
| macOS，映射 AOF（那里的默认）| 每次已返回的写入都在；6.4.0 报尾部损坏，把那段零（这次是 20 MB）挪进 `aof-<i>.aof.corrupt-quarantine.<ts>` 文件 | 每次已返回的写入都在，没有警告，没有隔离文件 |
| Linux，暂存环（那里的默认）| 还在环里的写入丢了（这次是 1,303,496 次里的 9,992 次）；6.4.0 不动 `.stage` 文件 | 每次已返回的写入都在 |

所以，**崩溃或被杀之后，先用 7.0 打开一次目录并干净地关闭，再退回 6.4**：这样暂存环会排空、零尾会截掉，留下的文件 6.4 照旧能读。在 Linux 和 Android 上，排空后的 `aof-<i>.aof.stage` 留在目录里，6.4 会忽略它。如果 6.4 已经打开过一个被杀后的目录并写入过，7.0 不会把过时的暂存环重放到这些更新的写入上面：它保留 6.4 写的内容，原来在环里的写入就丢了。

`Config::with_stage_ring(0)` 和 `Config::with_mapped_aof(false)` 可以关掉这两样；`AppendFsync::Always` 两样都不用。两样都关掉时，进程被杀会丢掉缓冲区里的内容，和 6.4 一样（实测这几次丢了 44 到 80 次写入）。

### 快照和重写过的日志

7.0 在每个 AOF 开头写一条内部记录，写明这份日志接着哪份快照，或者说明它是重写得到的完整镜像；每个快照文件末尾写上这份快照的 id；重启时只在接着某份快照的日志下面加载那份快照。6.4 两者都不认识：它加载目录里的快照，再把整份日志重放一遍。它读快照时在 id 之前就停了，重放时把那条记录算作一条命令并跳过。实测方法：用 7.0 在两个 shard 上写出目录（服务器和嵌入式存储各一次），再用 6.4.0 打开，对比列表、追加过的字符串、自增过的哈希字段和流：

| 7.0 停止前最后做的事 | 6.4.0 返回的和 7.0 一样吗 |
|---|---|
| 只有写入（没有快照） | 一样 |
| 一次快照，然后写入 | 一样 |
| 一次日志重写，目录里没有快照 | 一样 |
| 一次快照，然后一次日志重写，然后写入 | 不一样：重写之前 push 进列表的每个元素都出现两次；字符串、哈希和流是对的 |
| 在上一行之后再做一次快照 | 一样 |

出问题的那一行是 6.4 自己的规则：6.4 服务器执行 `BGSAVE` 之后再执行 `BGREWRITEAOF`，下次启动时同样会让这些列表翻倍（7.0 修掉了，见 [§16](#16-修掉的丢数据或改数据的缺陷)）。所以**退回之前，用 `SHUTDOWN SAVE` 停掉 7.0**（嵌入式存储：先调用 `save_snapshot()`，再关闭）：最后一步就是快照，日志里只有它之后的写入。实测：在一次快照和一次重写之后直接用 `SHUTDOWN SAVE` 停掉的服务器，用 6.4.0 打开，每个值都和 7.0 返回的一样。

### catalog 和消费者接触记录

日志里还有两处是新的，服务器和嵌入式存储都一样：

- 流消费组里消费者的接触记成内部的 `XINTERNAL.CONSUMERSEEN` 帧。6.4 会跳过它们，所以从日志重建消费组时，会丢掉只由 `XGROUP CREATECONSUMER` 创建的消费者；在快照里的消费者不受影响。
- 索引、视图和表的 catalog 不再存放在 `index-catalog.meta`、`view-catalog.meta` 和 `table-catalog.meta` 里。7.0 把每次改动记成日志里的一个内部 `XINTERNAL.CATALOG` 帧，帧里带着完整的 catalog；每份快照都保存当前的 catalog；7.0 第一次在 6.4 的目录上启动时，会把 catalog 从这三个文件里搬出来，然后删掉它们。

这对退回意味着什么，实测结果如下：

- 6.4.0 打开 7.0 写过的目录，不论数据来自日志还是快照，所有键都在，索引、视图和表一个都没有。
- 之后 6.4 写下的东西，7.0 都能读，catalog 也跟着回来；除非 6.4 重写过日志（`BGREWRITEAOF`），那会丢掉这些帧，7.0 再打开时用的是最新一份快照里的 catalog，没有快照就没有 catalog。
- 在 6.4 第一次打开目录之前，把升级前的三个附属文件复制回去，6.4 就能拿回它的 catalog。

所以退回之前，先留一份升级前的这三个文件，退回时先放回去；或者在 6.4 上重新声明 catalog。

## 2. 复制部署：先升级主节点

副本跨版本跟随主节点的键，两个方向都行。catalog 不跨版本：6.4 的主节点从来不把 catalog 发给副本，而 7.0 主节点发的帧 6.4 会跳过。实测用的是一个有两个索引、一张表和一个视图的 6.4.0 主节点，加一个自己声明过一个索引的 6.4.0 副本：

- **先升主节点。** 副本还在跑 6.4.0 时，保留自己声明的索引，主节点的一个也收不到。它第一次以 7.0 启动时，会在全量同步里拿到主节点的整个 catalog，自己声明的那个索引就没了。它的 `index-catalog.meta` 留在目录里。
- **先升副本。** 6.4.0 主节点的 7.0 副本没有索引、视图和表，而且拒绝声明（§3），所以在主节点跑上 7.0 之前，它上面的每个索引查询都以 `no such index` 失败。主节点随后以 7.0 重启时，会从附属文件读出 catalog 并通过复制流发出去，所以一直连着的副本追上之后就有了主节点的 catalog（实测：7.0 副本一直连着，6.4.0 主节点换成 7.0）。

两种顺序最后每个副本都会拿到主节点的 catalog；先升级主节点，副本没有索引的时间最短。在 6.4 下自己声明过索引的副本，要改成在主节点上声明（[复制](replication.md#取舍与限制)）。

## 3. 副本拒绝更多写入

只读副本对下面这些命令回复 `-READONLY You can't write against a read only replica.`，而 6.4 会在自己的键空间上执行它们，让副本和主节点渐渐不一致：

- `BLPOP`、`BRPOP`、`RENAME` 和 `RENAMENX`（6.4.0 实测：发给副本的 `BLPOP` 从副本那份列表里弹出了元素）；
- 所有 catalog 命令：`IDX.CREATE` / `DROP` / `REBUILD`、`VIEW.CREATE` / `DROP` / `REBUILD`、`TABLE.DECLARE` / `ENSURE` / `REPLACE` / `DROP`。

`EVAL_RO` 脚本也不能再调用前四个。嵌入式副本对 catalog 方法返回 `KevyError::ReadOnly`。索引、视图和表都在主节点上声明，每个副本都会收到。

## 4. `used_memory` 对同样的数据读数更高

6.4 给每个键在键空间表里的位置统一算 96 字节，哈希则少算了（哈希表外面那层包装从来没算，每个槽算 32 字节，实际占 49）。7.0 给键空间表和每个哈希算的是分配器为它们实际占着的字节，所以 `used_memory`、`MEMORY USAGE`、`maxmemory` 淘汰和分层存储的下沉看到的都是更大的真实数字。内存并没有多用，只是算进来了。

同样的 250,000 个键（200,000 个 32 字节的字符串，50,000 个四字段的哈希，其中一个字段 200 字节），两个 shard，实测：

| | 6.4.0 | 7.0 |
|---|---:|---:|
| `used_memory` | 69,200,000 | 103,543,040 |
| 进程 RSS | 220.8 MB | 207.7 MB |
| 一个字符串的 `MEMORY USAGE` | 128 | 200 |
| 一个哈希的 `MEMORY USAGE` | 872 | 1,272 |

所以按 6.4 的 `used_memory` 定的 `maxmemory`，数据量到原来的三分之二左右就开始淘汰。按 RSS 来定，或者按你在自己数据上测到的比例调高。

分层存储的服务器（`--tiering-budget`）现在还会看自己的常驻内存。如果实际内存连续两次读数都高于预算 × 1.05，每个 shard 都会拒绝让数据变大的写入，回复 `-OOM command not allowed when the process holds more memory than the tiering budget allows`，直到内存降回去。`INFO # Tiering` 多了 `tier_rss_line_bytes`、`tier_refusing_writes`、`tier_live_bytes` 和 `tier_overhead_bytes`。

## 5. 索引大小按实际报告

`IDX.LIST`、`IDX.VERIFY` 和 `TABLE.VERIFY` 里的 `bytes` 以前是每行 `value + key + 48`，只是个估算。现在报的是索引的叶子实际占用的字节，而索引本身比以前小得多：键形如 `row:<n>` 的 `i64` 索引，每行报 16–25 字节，6.4 报的是 67 字节，实际占用是 6.4 的四分之一或更少。同一个数字也用在索引的 `MAXMEM` 和分层存储给索引的预留上，因此：

- 按 6.4 定的 `MAXMEM`，在报 `-INDEXOVERBUDGET` 之前能装下几倍的行；
- 同样的索引旁边，分层存储能保持更多数据是热的。

有两种情况读数比 6.4 的估算高：

- 小索引：每个存有它的行的 shard，至少占一个 1,784 字节的叶子。实测：一行在 6.4.0 上报 59 字节，在 7.0 上报 1,816；两个 shard 上三行，分别是 186 和 3,632。几 KB 的 `MAXMEM` 现在可能导致构建失败。
- 值是长字符串、键又不是数字的索引，行按随机顺序写入之后，叶子只有 60–70% 满，直到后台重新打包处理到它们。

贴着上限设的预算，在装了真实数据的样本上对照 `IDX.LIST` 核一下。每行的公式见 [indexes.md](indexes.md#一致性与成本模型)。

## 6. `IDX.LIST` 与 `IDX.DESCRIBE` 各多一对

- `IDX.LIST` 的每一行末尾多了 `partitioning local|global`；全局索引还有 `partitions`、`max_entries` 和 `mean_entries`。
- `IDX.DESCRIBE` 在 `declaration` 之前给出 `partitioning`，`declaration` 仍是最后一对。

按键读这些回复的客户端会看到可以忽略的新键；按位置数的就不行了。

## 7. 嵌入式 `MSET` 每个 shard 一帧

嵌入式存储以前对 `MSET` 的每一对各加一次锁、各记一条 `SET`。现在一个 shard 的所有对在一把锁下设置，记成一条 `MSET` 帧，所以崩溃时每个 shard 的那一份要么全在要么全不在。AOF、副本和变更流原来看到的是每个键一条 `SET`，现在看到的是 `MSET` 帧；只处理 `SET` 的变更流消费者也要处理 `MSET`，面对服务器时本来就得这样。

流的写入记录方式也变了，服务器和嵌入式存储都一样，目的是让重放给出客户端当时拿到的回答：带生成 id 的 `XADD` 记录它给出的 id；认领（`XCLAIM`、`XAUTOCLAIM`）和组读取（`XREADGROUP`）记成针对所涉及条目的 `XCLAIM … FORCE JUSTID` 帧，读取还会加一条 `XGROUP SETID`，记下组前进到哪里；被满足的阻塞弹出记成它实际执行的 `LPOP` / `RPOP`。解读这些命令的变更流消费者看到的是记录下来的形式。

## 8. 端口已占用时拒绝启动

每个 shard 都用 `SO_REUSEPORT` 监听，这让同一用户在同一端口上起的第二个 kevy 并进了第一个的监听：两个都在跑，各分走一部分连接，写入看起来会从客户端读回的那一个上消失。现在端口被占用时启动会以 `Address already in use` 停下，和 Redis 一样。

## 9. io_uring：接收环变小，转发的工作处理完之后轮询

- io_uring reactor 上每个 shard 原来有 4,096 个 16 KiB 的接收缓冲区，每个 shard 64 MiB，流量循环过一遍之后全部常驻。现在数量由 `[advanced] recv_buffers` 设置，默认 1,024，每个 shard 16 MiB。环用完不算错误，接收会重新挂上。如果你测出确实需要，就设回 4096（[tuning.md](tuning.md)）。
- 处理完一批从其他 shard 转发来的工作后，shard 原来会睡 200 µs，期间收不到新输入，在两批流水线请求之间停顿的客户端每次都要等它睡完。现在改成在这 200 µs 里持续轮询，代价是每阵转发工作之后、shard 停下之前，最多占一个核 200 µs。在和其他进程共用的机器上，这会显示为 CPU 时间。

## 10. `kevy-config`：配置段结构体多了字段

加密链路和能放在代理后面的集群带来了新配置项，而每个配置项都是一个公开字段：

| 结构体 | 新字段 |
|---|---|
| `Config` | `secure`（类型为 `SecureSection`：`private_key_file`、`listen_port`、`client_keys`、`cluster_port_base`、`announce_cluster_port_base`）|
| `ClusterSection` | `announce_ip`、`announce_port_base`、`secure`、`peer_keys` |
| `PeerEntry` | `repl_port_base` |
| `ReplicationSection` | `secure`、`upstream_key`、`replica_keys` |
| `AdvancedSection` | `recv_buffers` |

把每个字段都写出来的结构体字面量会编译不过：

```rust
// 6.4
let repl = ReplicationSection { role, upstream, listen_port_base, /* … every field */ };
// 7.0
let repl = ReplicationSection { role, upstream, listen_port_base, ..Default::default() };
```

上表里除了 `PeerEntry` 都有默认值，`PeerEntry` 的新字段写 `repl_port_base: None` 就是原来的行为。用 `Config::load` 或 `Config::from_toml_str` 得到的配置不用改：新配置项默认都是关闭的，为 6.4 写的 `kevy.toml` 不改就能加载。

## 11. 给嵌入运行时的 Rust 用户

`kevy_rt::Commands` 多了 `take_ext_out`、`apply_ext` 和 `extension_targets`，默认实现都保持 6.4 的行为。它们在 shard 之间传递消息（一次写入要等这些消息都应用完才回复），并让一次扩展读取点名它需要的 shard。全局索引就建在它们之上。另外还多了 `snapshot_aux`、`load_snapshot_aux` 和 `on_restored`，默认实现不在键空间之外保存任何东西：命令集可以借它们把自己的状态存进每份快照和每个重写过的日志，并在所有 shard 恢复完之后统一处理一次。索引、视图和表的 catalog 就是这样保存的（§1）。

## 12. kevy-cli：工具只在 `--kevy` 后面

kevy-cli 6.4 以裸词运行工具，例如 `kevy-cli doctor -p 6004`、`kevy-cli export …`、`kevy-cli sql compile f.sql --apply --url h:p`，整个 6.x 期间都会打印一行提示，给出 `--kevy` 写法。7.0 里裸词就是服务端命令，和 redis-cli 一样，所以旧写法会把 `DOCTOR` 发给服务端，然后打印服务端的报错。把工具移到 `--kevy` 后面，连接选项放在它前面：

```sh
# 6.4
kevy-cli doctor -p 6004
kevy-cli sql compile schema.sql --apply --url h:p
# 7.0
kevy-cli -p 6004 --kevy doctor
kevy-cli -h h -p p --kevy sql compile schema.sql --apply
kevy-cli -p 6379 --kevy diff 127.0.0.1:6380 user:        # the first server is the session's
```

`digest <prefix>`，以及带文件参数的 `backup`/`restore`，改成 `--kevy digest|backup|restore`。为旧写法服务的库函数（`route_tool`、`run_doctor_cli`、`run_shadow_cli`、`run_lint_cli`、`run_backfill_keys_cli`）也一起删了。

## 13. Rust API

整个 workspace 的公开 Rust API 都按 [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) 整理过，把 kevy 的 crate 当库用的代码不改就编译不过。改动遵循下面几条规则，编译器会指出每一处：

- `bool` 参数换成按含义命名的枚举：`store.copy(a, b, true)` 写成 `store.copy(a, b, CopyMode::Replace)`；
- 以后可能扩充的结构体和枚举标了 `#[non_exhaustive]`：用 `Default` 或它的 `with_*` 构造方法，不再用结构体字面量；对它 `match` 要加 `_` 分支；
- 有不变量的类型字段私有，改用同名方法读取；
- 第一个参数是某个类型的自由函数，改成那个类型的方法；
- 错误都是实现了 `std::error::Error` 的类型，不再是 `String`；原本会发到协议上的错误文本，用 `as_wire()` 或 `to_wire()` 取到的还是原来那句；
- 变更流和复制里成对出现的（generation, offset）合成一个 `FeedPosition`。

嵌入式存储最常见的改法：

```rust
// 6.4
store.copy(b"src", b"dst", true)?;
store.linsert(b"l", true, b"c", b"b")?;
let (generation, offset) = store.changes_tail()?;
let batch = store.changes_since(generation, offset, 100, &[])?;
let dropped: bool = store.idx_drop(b"by_age");
// 7.0
store.copy(b"src", b"dst", CopyMode::Replace)?;
store.linsert(b"l", InsertPosition::Before, b"c", b"b")?;
let tail: FeedPosition = store.changes_tail()?;
let batch = store.changes_since(tail, 100, &[])?;  // batch.next is the next position
let dropped: bool = store.idx_drop(b"by_age")?;     // a closed store or a replica is an error now
```

`keys_iter` 返回一个 `KeysIter`，一次只持有一页，不再复制整个键空间。[rust-api-7.0.md](../rust-api-7.0.md) 按 crate 列出了每一处改动的旧写法和新写法（英文）。

## 14. 绑定：Go 模块路径和只读错误文本

- **Go。** 模块是 `github.com/goliajp/kevy-go/v7`。改掉 import 路径再 `go get`，Go API 的其他部分都没变。

  ```go
  // 6.4
  import kevy "github.com/goliajp/kevy-go/v6"
  // 7.0
  import kevy "github.com/goliajp/kevy-go/v7"
  ```

- **只读错误文本。** C++、C#、Go、Python、Tauri 和 TypeScript 绑定自己构造的只读错误，现在的文本是 `READONLY You can't write against a read only replica.`，和服务器的回复一字不差。其中五个原来少了末尾的句点，Tauri 插件原来说的是 `READONLY the store is a read-only replica`。匹配整条字符串的代码要换成新文本；匹配 `READONLY` 前缀或错误类型的代码不用改。
- **Android。** `KevyDB.mget` 改成一次原生调用，不再走命令路径执行 `MGET`；新增了 `KevyDB.mset(vararg pairs)`。结果不变。
- **Tauri。** 插件的 Rust `Error` 多了一个 `Other` 变体（它的 `kind()` 是 `"Other"`），pub/sub 事件的 `count` 是 `i64`，插件不认识的事件类型不再转发给 webview。JavaScript API 不变。
- 其他所有入口（Node、TypeScript、Electron、Expo、Nitro、Flutter、Python、C#、Java、Swift、C++、wasm 包）API 不变。嵌入式入口能观察到的引擎改动，是 §1、§7 和 §15 里的那些，再加上流和 geo 命令：嵌入式引擎现在通过通用命令路径执行它们（这条路径上 `BLOCK` 读取会被拒绝）。降级按 §1 做。

## 15. 变了的回复

下面每一项现在都和 Redis 的回复一致，或者去掉了原本就是错误的文本：

- `XRANGE`、`XREVRANGE` 以及其他接受流 id 的命令，拒绝 `5-` 这种短横线后面什么都没有的 id，回复 `ERR Invalid stream ID specified as stream command argument`。6.4 把它当成 `5-<最大序号>`。
- `XAUTOCLAIM` 的游标是下一个待处理条目的 id，列表扫完时是 `0-0`；6.4 返回的是最后扫到的 id 加一，所以扫到末尾的那次调用回复的是一个游标而不是 `0-0`。一次调用最多看 `COUNT × 10` 个条目，和 Redis 一样，6.4 会扫完整个列表。一直调用到游标为 `0-0` 的循环在两个版本上都能用，在 7.0 上少调一次。
- `EXPIRE`、`PEXPIRE`、`EXPIREAT` 和 `PEXPIREAT` 带非正 TTL，作用在截止时间恰好在命令执行期间到期的键上时，回复 0，什么都不记录；6.4 回复 1 并记录一次删除。
- `INFO replication` 报告 `repl_port_base`。
- 嵌入式存储的 `table_declare`、`table_replace` 和 `table_verify_report` 拒绝时回复 `-ERR …`，不再是 `-ERR ERR …`。
- `TABLE.DECLARE … WINDOW` 无法服务时的拒绝文本，中间多出的 17 个空格去掉了。
- 通过 `Store::dispatch_argv`，也就是每个语言绑定走的路径：`INCRBYFLOAT` 回复存储值本身的数字，和服务器一样；在已关闭的存储或副本上，格式不对的写入先按已关闭或 `READONLY` 拒绝，再检查参数；副本的 `READONLY` 回复以句点结尾，和服务器一样。

## 16. 修掉的丢数据或改数据的缺陷

下面每一项都可能在没有任何报错的情况下丢掉一次写入、一个截止时间或一个键，或者改掉一个值；括号里是最早出现这个缺陷的版本：

- 快照之后又重写了 AOF 时，重写之前 push 进列表的每个元素在重启后都被应用两次，因为重启在重写过的日志下面又加载了快照（服务器 1.0.0 起，嵌入式存储 1.16.0 起）；
- `COPY … REPLACE`（6.0.0 起）和跨 shard 的 `RENAME`（5.0.0 起）在重放和复制时写进目标键原有的值里，把哈希的字段或列表的元素合并进旧值；
- `BLPOP` 和 `BRPOP` 的弹出从来没有写进 AOF（1.4.0 起），也没有发给副本（1.18.0 起），所以弹出的元素在重启后又回来了；`BZPOPMIN`、`BRPOPLPUSH` 或 `XREADGROUP … BLOCK` 的等待方拿到数据后，同样没有记录；
- `BGSAVE` 之后写入的数据，在 macOS 和不用 io_uring 的 Linux 上（5.1.0 起）；
- AOF 重写最后交换文件的那一刻写入的数据（io_uring 上 5.0.0 起，epoll 和 kqueue 上 5.1.0 起）；
- 进程在事务中途死掉后写入的数据，在再下一次重启时丢失（4.0.0 起）；
- 后台 AOF 重写之后，哈希字段自己的 TTL（3.0.0 起）；
- `GETEX key EX|PX`（6.0.0 起）与带条件的 `HEXPIRE`（3.0.0 起），截止时间在重启时会移动；
- RESP3 下带数量的 `SPOP`，重启或副本重放时变成另一次随机弹出（6.3.0 起）；
- 流的写入，重启或副本重放的结果和当初的回复不一样：`XADD *` 生成了新的 id，`XCLAIM` 或 `XAUTOCLAIM` 认领了别的条目，消费组读取让每个待处理条目都像刚刚投递过（1.4.0 起；副本上 1.18.0 起）；
- 服务器记录一次相对 TTL 写入时 TTL 恰好到期的键，删除时既没有 `expired` 通知，也没有它的截止时间帧（1.8.1 起）；
- 在大键空间上紧跟着索引声明的物化视图，在 `VIEW.REBUILD` 之前只有部分行（3.0.0 起）；在已有数据上构建的带 `TOPK` 的 `DESC` 视图，每个 shard 留下的是最小的行而不是最大的行（3.0.0 起）；
- 没有经过命令就变了的行（过期、淘汰、脚本对另一个键的调用、副本的全量重新同步），它们的索引条目一直是旧的，直到之后某次写入点到这一行；
- 在不同 shard 上同时执行的 catalog 命令，两边都回复 `OK` 之后，可能丢掉对方的改动（3.0.0 起）；
- 嵌入式存储里：带 TTL 的键，过了截止时间后最多还能读到一个清理周期（1.11.0 起）；对字符串以外的类型做 `COPY`，回复 `WRONGTYPE`（2.0.13 起）；`SET … NX EX` 分两次加锁设置值和 TTL，两次之间崩溃会留下一个永不过期的键（4.0.0 起）；嵌入式副本会丢掉所有流和 geo 写入，而且（多于一个 shard 时）大多数键写进了读取时不会去找的 shard（1.22.0 起）。

[changelog](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md) 里有每一项的完整说明。

---

## 获取

```toml
# Cargo.toml
kevy-embedded = "7.0.0"
```

```sh
npm install @goliapkg/kevy@7.0.0          # wasm
npm install @goliapkg/kevy-node@7.0.0     # Node native
pip install kevy==7.0.0
go get github.com/goliajp/kevy-go/v7@v7.0.0
cargo install kevy --version 7.0.0        # the server binary, from source
npm install -g @goliapkg/kevy-bin@7.0.0   # the server binary, prebuilt
```

Linux（x86-64、arm64）和 macOS（arm64）的预编译服务器二进制，各附一个 SHA-256 文件，在 [v7.0.0 release](https://github.com/goliajp/kevy/releases/tag/v7.0.0) 上。用 `ghcr.io/goliajp/kevy:latest` 的容器用户下次拉取就是 7.0.0；固定版本的 tag 是 `ghcr.io/goliajp/kevy:7.0.0`。
