# 集群

kevy 的集群面有两个相互独立的层 —— **单节点多 shard 暴露**(一个进程,每个 shard 都讲 Redis Cluster 协议)与**多节点复制 + 作用域多写者**(主、副本、embed、多数派切换)—— 你可以只用一层、两层都用,或都不用。

## 两层概览

**单节点集群模式。** 一个 kevy 进程把键空间分成 N 个 shard,并把每个 shard 作为一个虚拟集群节点暴露到一个确定的 per-shard 端口。`CLUSTER SLOTS / SHARDS / NODES` 报告真实的 CRC16 分区;键感知客户端(`redis-cli -c`、`redis-benchmark --cluster`、stock 集群感知库,以及内置的 [`ClusterClient`](https://github.com/goliajp/kevy/blob/master/crates/kevy-client/examples/cluster.rs))为每个键计算槽位,直接连到拥有它的 shard。收益是机械的 —— 去掉服务端跨 shard 跳转,直接转化成更高吞吐与更低尾延迟。

**多节点集群。** 一台 kevy 服务器可以作为**主节点**把写入日志流式发到一个或多个**副本**(可以是 kevy 服务器,也可以是进程内的 [`kevy-embedded`](https://github.com/goliajp/kevy/tree/master/crates/kevy-embedded) store)。主节点也可以按前缀把**作用域写入**委派出去:`[cluster] scopes` 声明谁拥有 `app:billing:*` 的写、谁拥有 `app:auth:*` 的写等;打到错误节点上的写会得到 `-MISDIRECTED writer is <host:port>` 让客户端跟进。[`kevy-elect`](https://github.com/goliajp/kevy/tree/master/crates/kevy-elect) 提供多数派心跳,把写者标记为 DOWN 并提升声明的备用节点。运维侧的 `MOVE-SCOPE` 在有界静默窗口中迁移一个前缀。

## 何时需要

| 情境 | 选用 |
|------|------|
| 单进程,键感知客户端,想去掉跨 shard 跳 | 单节点集群模式 + `ClusterClient` |
| 同一台主机上要兼容 stock Redis Cluster 工具 | 单节点集群模式 |
| 热读由另一台机器或进程内承担 | 多节点:主 + 副本(或以 embed 作副本) |
| 多写者,按键前缀切分,部署在不同主机 | 多节点:作用域多写者 |
| 在没有人工介入的情况下扛住写者崩溃 | 多节点:`kevy-elect` + 作用域回退 |
| 单进程、低负载、普通客户端 | 都不用 —— 默认代理端口就够 |

两层可组合:集群模式的主节点对外宣称 N 个 shard,每个副本也跑 N 个 shard,路由客户端把它们连起来。

---

# Layer 1 —— 单节点集群模式

## 核心思路

一个普通的 kevy 进程在单一端口上接受所有命令,并在内部把错路由的键转发到拥有它的 shard。这种转发是正确的,但在热路径上会主导 p99 延迟并压住吞吐。集群模式把每个 shard 暴露在它自己的端口上;键感知客户端用 CRC16-XMODEM 计算键的哈希,从 `CLUSTER SLOTS` 查到拥有者 shard,直接连过去 —— 没有转发,没有 `-MOVED`。

```
                  ┌─────────────────────────────────────────┐
                  │            kevy process (1 host)        │
                  │                                         │
  main port  ───▶ │  6004  ── proxy: forwards or -MOVED ──▶ │
                  │                                         │
  shard ports ──▶ │  6005  ── shard 0   (slots     0– 4095) │
                  │  6006  ── shard 1   (slots  4096– 8191) │
                  │  6007  ── shard 2   (slots  8192–12287) │
                  │  6008  ── shard 3   (slots 12288–16383) │
                  └─────────────────────────────────────────┘
```

shard `i` 总是绑定到 `port_base + 1 + i`(在 TOML 里可覆盖 `port_base`)。主端口为不会讲集群协议的客户端保留代理行为;per-shard 端口在键到达错误持有者时回答 `-MOVED <slot> <host:port>`。

整键空间命令(`KEYS`、`SCAN`、`DBSIZE`、`FLUSHALL`)在所有端口上仍然作用于整个键空间 —— kevy 在内部做扇出,客户端不必管。

## 启用

```toml
# kevy.toml
port = 6004

[cluster]
enabled   = true
# port_base = 6004   # 默认与 `port` 相同;shard 在 port_base + 1 + i
```

等价的 CLI / env:

```sh
kevy --port 6004 --threads 8 --cluster      # shard 端口 6005..6012
KEVY_CLUSTER=1 kevy --port 6004 --threads 8
```

把数据目录切入或切出集群模式会在启动时把键重新归位一次;原文件备份为 `*.premigration.<ts>`。

## 在 Rust 中使用 `ClusterClient`

```toml
[dependencies]
kevy-client = "*"
```

```rust
use kevy_client::ClusterClient;

// 用任一集群端口作种子;拓扑通过 CLUSTER SLOTS 发现,
// 然后每个 shard 一个连接。
let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;

cc.set(b"user:42", b"alice")?;
let v = cc.get(b"user:42")?;            // 路由到 user:42 所属 shard
let n = cc.incr(b"counter")?;

// 多键 DEL/EXISTS —— 按键路由后求和。
let removed = cc.del(&[b"a", b"b", b"c"])?;
# Ok::<(), std::io::Error>(())
```

可运行的种子示例在 [`crates/kevy-client/examples/cluster.rs`](https://github.com/goliajp/kevy/blob/master/crates/kevy-client/examples/cluster.rs);基准在 [`crates/kevy-client/examples/cluster_bench.rs`](https://github.com/goliajp/kevy/blob/master/crates/kevy-client/examples/cluster_bench.rs)。

### 路由如何去掉跨 shard 跳

1. **发现。** `connect` 向种子发送 `CLUSTER SLOTS`,读出每个 shard 的 `[start, end, host, port]`,然后构建一张 16384 项的 `slot → shard-index` 表。这张表来自服务器宣称的范围,所以客户端不必复刻分区算术。
2. **路由。** 每条单键命令计算 `key_hash_slot(key)`(对 `{hashtag}` 做 CRC16-XMODEM,若无 hashtag 则对整键),然后直接发到该 slot 拥有者的连接。
3. **必要时扇出。** `dbsize`、`flushall` 与其他整集群命令在服务端处理;客户端只发一次调用。

在一台 16 核 lx64 上做并发 64 的 GET,去掉跨 shard 跳让实测吞吐从 333 k ops/s 提升到 533 k ops/s(1.6×),p99 从 3858 µs 降到 260 µs(尾部约低 15×)。用 `cargo run -p kevy-client --release --example cluster_bench` 复现。

> 跨 shard 跳的代价只在负载充足的干净机器上才会显现。在小型同地云 VM 上,这个差距会被调度噪声吞掉。

### 跨 slot 的多键命令

与 Redis Cluster 不同,kevy 在单节点集群上对跨 shard 的多键命令(`MGET`、`MSET`、`SUNION`、事务、阻塞扇出)**不会**返回 `-CROSSSLOT`:服务器跨 shard 完成请求。kevy 在单机上是 Redis Cluster 的超集 —— 每一个 Redis Cluster 客户端都能用,而原本会撞 `-CROSSSLOT` 的接口面也仍然可用。共享的 `{hashtag}` 在需要原子性的数据共位时仍是合适的工具,但不再是正确性的硬要求。

### 集群端口上支持的 `CLUSTER` 命令

| 命令 | 行为 |
|------|------|
| `CLUSTER SLOTS` | 真实分区:每 shard 一行 `[start, end, host, port]`。 |
| `CLUSTER SHARDS` | 同一份数据的新风格,仅主节点。 |
| `CLUSTER NODES` | 扁平文本清单,每 shard 一行,ID 由 shard 索引派生。 |
| `CLUSTER MYID` | 回答调用的 shard 的确定 ID。 |
| `CLUSTER KEYSLOT <key>` | 对 `{hashtag}` 或整键做 CRC16-XMODEM。 |
| `CLUSTER COUNTKEYSINSLOT <slot>` | 走该 shard 的索引实时计数。 |
| `CLUSTER COUNT-FAILURE-REPORTS <id>` | 永远 0 —— 本层没有故障检测器。 |
| `CLUSTER INFO` | 报告 `cluster_enabled:1`、`cluster_state:ok`、slot 覆盖率。 |
| `CLUSTER RESET`、`CLUSTER FORGET`、`CLUSTER MEET`、`CLUSTER FAILOVER`、`MIGRATE`、`ASK` | 未实现 —— 见 *不在范围内*。 |

### 回退到原始路由助手

```rust
// 把任意单键命令路由到它的拥有者 shard。
let reply = cc.request_keyed(b"mykey", &[b"STRLEN".to_vec(), b"mykey".to_vec()])?;
// 无键命令发到任一 shard。
let reply = cc.request_unkeyed(&[b"PING".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

`ClusterClient` 封装了字符串、哈希、列表、集合、有序集合、pub/sub 的常用动词,以及多键 `DEL` / `EXISTS`。Pub/sub 在进程层面全局:任一端口上的 `Subscriber` 会收到所有发布的消息,无论是哪个 shard 接收了 `PUBLISH`。

---

# Layer 2 —— 多节点集群

## 主节点与副本

一台 kevy 服务器可以是主节点(默认)、镜像主节点写日志的副本,或者同时是两者(级联)。主节点开一个专用的复制监听;副本连上去,交出它最后施加的 offset,然后把流过来的帧施加到本地 shard 上。

```toml
# primary.toml
port = 6004

[replication]
listen_port = 16004        # 主节点在这里流出日志
```

```toml
# replica.toml
port = 6004

[replication]
upstream    = "primary.local:16004"
replica_id  = "replica-eu-1"           # 每个副本稳定;穿越重启
# reconnect_min_ms = 100               # 退避包络
# reconnect_max_ms = 5000
```

完整的服务器侧语义 —— backlog 容量、快照吸收、级联 —— 都在 [`docs/replication.md`](replication.md) 里。对本文档相关的事实是:同一份线协议承载集群模式的复制 —— 跑着 `[cluster] enabled = true` 的主节点流出 N 个 shard 的写入,而跑着同样 shard 数的副本逐 shard 施加。

## 以 embed 作只读副本

一个 [`kevy-embedded`](https://github.com/goliajp/kevy/tree/master/crates/kevy-embedded) store 可以直接订阅主节点,以零网络跳的方式在进程内服务读。本地写以 `READONLY` 拒绝。

```rust
use kevy_embedded::Store;

// 内存中的副本,AOF 关,默认重连(100 ms → 5 s)。
let replica = Store::open_replica("primary.local:16004")?;

let v = replica.get(b"hello")?;
assert!(replica.set(b"k", b"v").is_err());      // READONLY
# Ok::<(), std::io::Error>(())
```

调优:

```rust
use std::time::Duration;
use kevy_embedded::{Config, Store};

let cfg = Config::default()
    .with_replica_upstream("primary.local:16004")
    .with_replica_id("backup-svc-region-a")
    .with_replica_reconnect(Duration::from_millis(50), Duration::from_secs(10));
let replica = Store::open(cfg)?;
# Ok::<(), std::io::Error>(())
```

握手发送 `REPLICATE FROM <last-applied-offset> ID <replica_id>`;主节点 ack offset 然后流出帧。最后一个 `Store` clone drop 时 runner 线程会被 join,因此主节点观察到一个干净的 FIN 并释放 slot。embed 上允许本地 `PUBLISH`(pub/sub 是进程局部的),但键空间本身保持只读。

## 作用域多写者

作用域多写者按键前缀把写入切分到各节点。每个节点都从静态配置知道完整的归属表;打到非拥有者的写会回答 `-MISDIRECTED writer is <host:port>`,客户端据此向正确节点重试。

```toml
# 同一段 cluster 配置写到每个成员上。
[cluster]
node_id = "embed-billing-1"
peers   = "embed-billing-1@10.0.0.1:6004,server-eu-1@10.0.0.2:6004,reader-1@10.0.0.3:6004"

# prefix=writer[|fallback],逗号分隔。
# 第一个 `=` 切分前缀与拥有者声明,所以 `app:billing:`(含 `:`)没问题。
scopes  = "app:billing:=embed-billing-1|server-eu-1, app:auth:=embed-auth-1"

elect_port_base = 16100    # kevy-elect 监听在这里
```

`peers` 是一串扁平的 `<node_id>@<host>:<port>` —— 无嵌套结构,易于模板化。`scopes` 解析为 `prefix=writer[|fallback]`,逗号分隔。不拥有任何作用域的节点直接转发写入;拥有某作用域的节点接受该作用域的写,拒绝其余。

读与作用域归属无关 —— 任何持有数据的节点(通常是只读副本)都可服务读。作用域机制只用于写入归属。

### 以 embed 作作用域写者

```rust
use kevy_embedded::{Config, Store};

let writer = Store::open(
    Config::default().with_embed_writer("0.0.0.0:6105")
)?;

// 本地写入喂给 embed 的复制源 backlog;
// 读者通过 kevy_replicate::ReplicaClient 连到 0.0.0.0:6105。
writer.set(b"app:billing:invoice:42", b"...")?;
# Ok::<(), std::io::Error>(())
```

embed 在传给 `with_embed_writer` 的地址上开一个复制监听。其它节点从那里拉日志,跟从服务器主节点拉日志完全一样。

## `kevy-elect` 多数派切换

`kevy-elect` 是集群每个成员都跑的心跳侧车。每个节点在选举端口(`elect_port_base + node_index`)发心跳;每个节点维护一份滑动窗口记录谁最近还活着。当一个 peer 的最后心跳超过 `down_after`(默认 5 秒),它进入 `down_peers`。一个作用域声明的回退节点会在每次接受的写上检查 `down_peers`:如果它的写者是 DOWN,回退节点就把自己当作生效拥有者并接受写;之后其它节点上的写都会 MISDIRECT 到这个回退节点。当原写者的心跳恢复后,它离开 `down_peers`,在下一次决策时回退节点隐式让位。

| 旋钮 | 含义 | 默认值 |
|------|------|--------|
| `node_id` | 本节点稳定标识(`<scope_owner>` 引用与此匹配)| 必填 |
| `peers` | 集群每个成员的 `<node_id>@<host>:<port>` 列表 | 必填 |
| `elect_port_base` | 本地 elect 侧车绑定的 UDP 端口 | `16100` |
| `hb_interval_ms` | 心跳发送节拍 | `500` |
| `down_after_ms` | peer 在多少毫秒无心跳后被标 DOWN | `5000` |

### 手工 rejoin 恢复

如果原写者 DOWN 时间足够久让回退节点接受了写,那些写只活在回退节点上。在重新启用原写者承担该作用域之前:停掉写者,把回退节点的数据目录拷贝到写者的目录,再重启。这与"无共识"契约一致 —— 不偷偷写、不双重接受。

---

# `MOVE-SCOPE`

`MOVE-SCOPE` 在有界静默窗口中把一个前缀从一个写者迁到另一个写者。它由运维方下发,运行在当前写者上。

```
MOVE-SCOPE <prefix> from <from-node-id> to <to-node-id>
```

逐步:

1. 当前写者把 `<prefix>` 的本地状态翻为 MIGRATING。该前缀下键的后续写返回 `-QUIESCED migrating to <to-host:port>`。客户端短暂退避后重试。
2. 写者把该前缀的键空间切片序列化,通过 `MOVE-SCOPE-INGEST <prefix> <bulk>` 推送到目标的数据端口。
3. 目标回 `+OK` 后,写者在本地提交迁移。源上后续对该前缀的写返回 `-MISDIRECTED writer is <to-host:port>`。
4. 集群其它成员仍按各自静态 `scopes` 配置路由,直到运维方推送新配置并重启它们。

迁移过程中你会看到两种线协议回复:

| 回复 | 含义 |
|------|------|
| `-MISDIRECTED writer is <host:port>` | 写打到非拥有者。向指名的主机重试。 |
| `-QUIESCED migrating to <host:port>` | MOVE-SCOPE 窗口内的瞬时态。退避后重试。 |

集群感知客户端在收到 `-MISDIRECTED` 时缓存每键的目标并透明重试;在 `-QUIESCED` 时应短暂 sleep(几百毫秒数量级)再重试。

中途中止会回退到源写者;目标上不会留下半施加状态。

---

# 配置参考

## 单节点集群模式

| TOML | CLI | Env | 默认 | 含义 |
|------|-----|-----|------|------|
| `[cluster] enabled` | `--cluster` | `KEVY_CLUSTER=1` | `false` | 把每个 shard 暴露在 per-shard 端口。 |
| `[cluster] port_base` | `--cluster-port-base` | `KEVY_CLUSTER_PORT_BASE` | `port` 的值 | shard `i` 绑定 `port_base + 1 + i`。 |

## 复制(主侧)

| TOML | CLI | Env | 默认 |
|------|-----|-----|------|
| `[replication] listen_port` | `--replication-listener` | `KEVY_REPLICATION_LISTEN_PORT` | 未设(关)|

## 复制(副本侧)

| TOML | CLI | Env | 默认 |
|------|-----|-----|------|
| `[replication] upstream` | `--replicate-from` | `KEVY_REPLICATE_FROM` | 未设 |
| `[replication] replica_id` | `--replica-id` | `KEVY_REPLICA_ID` | 从主机名派生 |
| `[replication] reconnect_min_ms` | | | `100` |
| `[replication] reconnect_max_ms` | | | `5000` |

## 作用域多写者 + 选举

| TOML | 含义 |
|------|------|
| `[cluster] node_id` | 本节点稳定标识。 |
| `[cluster] peers` | 集群每个成员的 `<node_id>@<host>:<port>` 列表。 |
| `[cluster] scopes` | `prefix=writer[\|fallback]` 条目,逗号分隔。 |
| `[cluster] elect_port_base` | 本地 elect 侧车绑定的 UDP 端口。 |
| `[cluster] hb_interval_ms` | 心跳发送节拍(默认 `500`)。 |
| `[cluster] down_after_ms` | peer 在多少毫秒无心跳后被标 DOWN(默认 `5000`)。 |

---

# 取舍与限制

- **单节点集群模式还是单进程。** 它买到的是客户端侧键路由,而不是主机级容错。要容错请加副本。
- **代理端口仍然可用。** 它对非集群客户端继续工作并保持正确,只是多了跨 shard 跳。
- **拓扑是静态的。** `peers` 与 `scopes` 启动时从配置读取。变更靠"推新配置,重启"。设计上没有 gossip。
- **`MOVE-SCOPE` 会静默化该前缀的写入。** 窗口受切片发送时间限制,GB 级作用域在 LAN 上是个位数秒。远大于此的前缀请安排在维护窗口。
- **以 embed 作作用域写者面向服务形态负载**(账单服务、认证服务),不面向多 TB 数据集。
- **回退接受写之后的手工 rejoin 恢复。** 在重新启用前把回退节点的数据目录拷给写者;没有自动共识追赶。

---

# 设计上不在范围内

- AUTH 与 TLS —— 由部署边缘(sidecar、mesh、LB)处理,不在 kevy 内。
- 跨数据中心多活与 CRDT。
- 键空间下的 Raft、Paxos 或任何共识日志。
- 基于 gossip 的发现 —— `peers` 是静态的。
- 在线 reshard、`MIGRATE`、`ASK` 重定向。
- 拥有重叠的多主 —— 每个前缀同一时刻恰好有一个写者。

这些不会被加入。简单本身就是特性。

---

# FAQ

**用复制必须用集群模式吗?**
不必。单节点集群模式与复制/多节点层是独立的。非集群主可以带非集群副本;集群主也可以带集群副本。它们能组合,但谁都不强求谁。

**我能用标准的集群感知客户端(Lettuce、ioredis、redis-py-cluster)连 kevy 的集群模式吗?**
能。`CLUSTER SLOTS / SHARDS / NODES` 宣称的是真实分区,错 shard 时返回 `-MOVED`,这正是那些库唯一依赖的接口面。注意连 per-shard 端口(而不是主代理端口),客户端的路由才能真的到达 shard。

**单节点集群模式下,跨 shard 的多键命令会怎样?**
成功。kevy 在服务端跨 slot 执行 `MGET`、`MSET`、`SUNION`、事务与阻塞扇出,而不是返回 `-CROSSSLOT`。`{hashtag}` 共位对于原子敏感场景仍然有用,但不再是正确性硬要求。

**没有运维介入时如何扛过写者崩溃?**
为作用域声明一个回退(`prefix=writer|fallback`)并在每个节点跑 `kevy-elect`。当写者心跳错过 `down_after_ms`,回退开始接受该前缀的写;客户端收到 `-MISDIRECTED writer is <fallback>` 后跟进。原写者回来后,执行手工 rejoin 恢复。

**为什么 gossip / Raft 永远不在范围内?**
在每次写下塞共识日志的成本,会抹掉让 kevy 值得选用的吞吐与尾延迟优势。静态配置 + 多数派心跳的设计给你切换分支,而不必在热路径上付出状态机复制的代价。如果你的负载真的需要一个共识支撑的 KV,kevy 就是错的工具。
