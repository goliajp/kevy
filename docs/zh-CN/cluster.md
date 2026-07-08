# 集群

kevy 的集群能力分两层，彼此独立——**单节点多 shard 暴露**（一个进程，每个 shard 都讲 Redis Cluster 协议）和**多节点复制 + 作用域多写者**（主节点、副本、embed、多数派切主）——可以只开一层、两层都开，或都不开。

## 两层概览

**单节点集群模式。**一个 kevy 进程把键空间切分成 N 个 shard，并把每个 shard 作为虚拟集群节点暴露在各自确定的端口上。`CLUSTER SLOTS / SHARDS / NODES` 报告的是真实的 CRC16 分区；键感知客户端（`redis-cli -c`、`redis-benchmark --cluster`、现成的集群感知库，以及自带的 [`ClusterClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client/examples/cluster.rs)）对每个键做哈希，直连持有它的 shard。这份收益是机械式的——省掉服务端的跨 shard 一跳，直接换来更高的吞吐和更低的尾延迟。

**多节点集群。**一台 kevy 服务器可以充当**主节点**，把写日志流式发给一个或多个**副本**（副本既可以是 kevy 服务器，也可以是进程内的 [`kevy-embedded`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-embedded) store）。主节点还能按前缀委派**作用域写入**：`[cluster] scopes` 声明 `app:billing:*` 的写归哪个节点、`app:auth:*` 的写归哪个节点，依此类推；写请求落到错误节点时会收到 `-MISDIRECTED writer is <host:port>`，客户端据此改投正确节点。[`kevy-elect`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-elect) 提供多数派控制面：把死掉的节点标成 DOWN，为复制拓扑选出接任的主节点（v3.15），并提升作用域声明的回退节点。运维下发 `MOVE-SCOPE`，可在静默窗口内迁移一个前缀。

## 何时需要

| 场景 | 选什么 |
|------|--------|
| 单进程，客户端键感知，想省掉跨 shard 一跳 | 单节点集群模式 + `ClusterClient` |
| 单机上兼容现成的 Redis Cluster 工具链 | 单节点集群模式 |
| 热点读放到另一台机器或进程内来服务 | 多节点：主节点 + 副本（或以 embed 作副本） |
| 多个写者，按键前缀切分，部署在不同主机 | 多节点：作用域多写者 |
| 写者崩溃也不需要人工介入 | 多节点：`kevy-elect` 多数派选举（主节点）/ 作用域回退（作用域写者） |
| 单进程、低负载、普通客户端 | 都不用——默认代理端口就够了 |

两层可以组合：集群模式的主节点对外通告 N 个 shard，每个副本同样跑 N 个 shard，由路由客户端把两边接起来。

---

# Layer 1——单节点集群模式

## 核心思路

普通的 kevy 进程在单个端口上接下所有命令，内部再把走错门的键转发给持有它的 shard。这个转发在正确性上没有问题，但在热路径上会主导 p99 延迟，也压住了吞吐上限。集群模式把每个 shard 暴露在各自的端口上；键感知客户端用 CRC16-XMODEM 对键做哈希，从 `CLUSTER SLOTS` 查出持有者 shard，直接连过去——没有转发，也没有 `-MOVED`。

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

shard `i` 固定绑定 `port_base + 1 + i`（`port_base` 可在 TOML 里覆盖）。主端口保留代理行为，服务不讲集群协议的客户端；per-shard 端口在键落错持有者时应答 `-MOVED <slot> <host:port>`。

全键空间命令（`KEYS`、`SCAN`、`DBSIZE`、`FLUSHALL`）在任何端口上都仍作用于整个键空间——kevy 在内部完成扇出，客户端无需自己动手。

## 启用方式

```toml
# kevy.toml
port = 6004

[cluster]
enabled   = true
# port_base = 6004   # defaults to `port`; shards live at port_base + 1 + i
```

等价的 CLI / env：

```sh
kevy --port 6004 --threads 8 --cluster      # shard ports 6005..6012
KEVY_CLUSTER=1 kevy --port 6004 --threads 8
```

数据目录切进或切出集群模式时，键会在启动时一次性重新归位；先前的文件备份为 `*.premigration.<ts>`。

## 在 Rust 里使用 `ClusterClient`

```toml
[dependencies]
kevy-client = "*"
```

```rust
use kevy_client::ClusterClient;

// Seed against any cluster port; topology is discovered via CLUSTER SLOTS
// and one connection is opened per shard.
let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;

cc.set(b"user:42", b"alice")?;
let v = cc.get(b"user:42")?;            // routed to user:42's owner shard
let n = cc.incr(b"counter")?;

// Multi-key DEL/EXISTS — routed per key and summed.
let removed = cc.del(&[b"a", b"b", b"c"])?;
# Ok::<(), std::io::Error>(())
```

可直接运行的种子示例在 [`crates/kevy-client/examples/cluster.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client/examples/cluster.rs)；配套基准在 [`crates/kevy-client/examples/cluster_bench.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client/examples/cluster_bench.rs)。

### 路由如何省掉跨 shard 一跳

1. **发现。**`connect` 向种子端口发送 `CLUSTER SLOTS`，读出每个 shard 的 `[start, end, host, port]`，构建一张 16384 项的 `slot → shard-index` 表。这张表来自服务器通告的范围，客户端因此不必自己重新实现分区算术。
2. **路由。**每条单键命令计算 `key_hash_slot(key)`（存在 `{hashtag}` 时对其做 CRC16-XMODEM，否则对整个键），直接发到该 slot 持有者的连接上。
3. **必要时扇出。**`dbsize`、`flushall` 等全集群命令由服务端处理；客户端只发一次调用。

在 16 核 lx64 机器上以并发 64 跑 GET，省掉跨 shard 一跳后，实测吞吐从 333 k ops/s 升到 533 k ops/s（1.6×），p99 从 3858 µs 降到 260 µs（尾延迟约低 15 倍）。复现命令：`cargo run -p kevy-client --release --example cluster_bench`。

> 这一跳的开销只有在干净机器上压足负载才看得见。在同宿主的小型云 VM 上，差距会被调度噪声淹没。

### 跨 slot 的多键命令

与 Redis Cluster 不同，单节点集群上的多键命令（`MGET`、`MSET`、`SUNION`、事务、阻塞扇出）跨 shard 时，kevy **不会**返回 `-CROSSSLOT`：服务端直接跨 shard 完成请求。在单机上，kevy 是 Redis Cluster 的超集——所有 Redis Cluster 客户端都能用，原本会撞上 `-CROSSSLOT` 的那部分接口面也照样可用。需要把数据放在一起以保证原子性时，共享 `{hashtag}` 仍是合适的工具，只是它不再是正确性的硬要求。

### 集群端口上支持的 `CLUSTER` 命令

| 命令 | 行为 |
|------|------|
| `CLUSTER SLOTS` | 真实分区：每个 shard 一行 `[start, end, host, port]`。 |
| `CLUSTER SHARDS` | 同一份数据的新版格式，只列主节点。 |
| `CLUSTER NODES` | 扁平文本清单，每个 shard 一行，ID 由 shard 索引推导。 |
| `CLUSTER MYID` | 应答本次调用的 shard 的确定性 ID。 |
| `CLUSTER KEYSLOT <key>` | 对 `{hashtag}` 或整个键做 CRC16-XMODEM。 |
| `CLUSTER COUNTKEYSINSLOT <slot>` | 遍历持有该 slot 的 shard 的索引，实时计数。 |
| `CLUSTER COUNT-FAILURE-REPORTS <id>` | 恒为 0——这一层没有故障检测器。 |
| `CLUSTER INFO` | 报告 `cluster_enabled:1`、`cluster_state:ok` 与 slot 覆盖情况。 |
| `CLUSTER RESET`、`CLUSTER FORGET`、`CLUSTER MEET`、`CLUSTER FAILOVER`、`MIGRATE`、`ASK` | 未实现——见*设计上不在范围内*。 |

### 退回原始路由助手

```rust
// Route an arbitrary single-key command to its owner shard.
let reply = cc.request_keyed(b"mykey", &[b"STRLEN".to_vec(), b"mykey".to_vec()])?;
// Keyless commands go to any shard.
let reply = cc.request_unkeyed(&[b"PING".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

`ClusterClient` 封装了字符串、哈希、列表、集合、有序集合、pub/sub 的常用 verb，外加多键 `DEL` / `EXISTS`。pub/sub 是进程级全局的：任一端口上的 `Subscriber` 都能看到全部已发布消息，与哪个 shard 接下 `PUBLISH` 无关。

---

# Layer 2——多节点集群

## 主节点与副本

一台 kevy 服务器可以当主节点，把自己的写日志流式发出；也可以当副本，镜像某个主节点（默认角色是 `standalone`，复制子系统休眠）。主节点为每个 shard 绑定一个专用复制监听；副本连上来，报出自己最后应用的 offset，再把流过来的帧逐条应用到本地 shard。不支持链式复制（副本的副本）。

```toml
# primary.toml
port = 6004

[replication]
role             = "primary"
listen_port_base = 16004    # optional; defaults to port + 10000
```

```toml
# replica.toml
port = 6004

[replication]
role     = "replica"
upstream = "primary.local:16004"
```

完整的服务端语义——backlog 容量、快照导入、心跳与 ACK——见 [`docs/replication.md`](replication.md)；切主（计划内 `FAILOVER` 与崩溃选举）与一致性阶梯见 [`docs/availability.md`](availability.md)。与本文相关的事实只有一条：集群模式的复制走同一套线协议——开着 `[cluster] enabled = true` 的主节点流出 N 个 shard 的写入，shard 数相同的副本逐 shard 应用。

## 以 embed 作只读副本

[`kevy-embedded`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-embedded) store 可以直接订阅主节点，在进程内提供零网络跳的读服务。本地写一律以 `READONLY` 拒绝。

```rust
use kevy_embedded::Store;

// In-memory replica, AOF off, default reconnect (100 ms → 5 s).
let replica = Store::open_replica("primary.local:16004")?;

let v = replica.get(b"hello")?;
assert!(replica.set(b"k", b"v").is_err());      // READONLY
# Ok::<(), std::io::Error>(())
```

需要调参时：

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

握手时发送 `REPLICATE FROM <last-applied-offset> ID <replica_id>`；主节点确认该 offset 后开始流式发送帧。最后一个 `Store` 克隆 drop 时会 join runner 线程，主节点因此能观察到干净的 FIN 并释放连接槽位。embed 本地允许 `PUBLISH`（pub/sub 是进程内局部的），但键空间本身保持只读。

## 作用域多写者

作用域多写者按键前缀把写入切分到不同节点。每个节点都从静态配置得到完整的归属表；写请求落到非持有者时，应答 `-MISDIRECTED writer is <host:port>`，客户端改向正确节点重试。

```toml
# Same config block on every member.
[cluster]
node_id = "embed-billing-1"
peers   = "embed-billing-1@10.0.0.1:6004,server-eu-1@10.0.0.2:6004,reader-1@10.0.0.3:6004"

# prefix=writer[|fallback], comma-separated.
# The first `=` splits prefix from owner spec, so `app:billing:` (with `:`) is fine.
scopes  = "app:billing:=embed-billing-1|server-eu-1, app:auth:=embed-auth-1"

elect_port_base = 16100    # kevy-elect listens here
```

`peers` 是一串 `<node_id>@<host>:<port>` 条目组成的扁平字符串——没有嵌套结构，便于模板化。`scopes` 按 `prefix=writer[|fallback]` 解析，逗号分隔。不持有任何作用域的节点只是把写转发出去；持有某个作用域的节点接受该作用域的写，拒绝其余的写。

读与作用域归属无关——持有数据的任何节点（通常是只读副本）都能服务读。作用域机制只管写入归属。

### 以 embed 作作用域写者

```rust
use kevy_embedded::{Config, Store};

let writer = Store::open(
    Config::default().with_embed_writer("0.0.0.0:6105")
)?;

// Local writes feed the embed's replication source backlog;
// readers connect to 0.0.0.0:6105 via kevy_replicate::ReplicaClient.
writer.set(b"app:billing:invoice:42", b"...")?;
# Ok::<(), std::io::Error>(())
```

embed 会在传给 `with_embed_writer` 的地址上开一个复制监听。其他节点从这里拉日志，和从服务器主节点拉完全一样。

## `kevy-elect` 多数派切主

`kevy-elect` 是进程内的多数派控制面，只要同时设置了 `[cluster] node_id` 和 `peers`，每个集群成员都会运行它。每个节点在 `elect_port_base`（默认：客户端端口 + 200）绑定一个 TCP 控制监听，并向所有 peer 发心跳；peer 静默超过 `down_after`（5 s）就标成 DOWN。成员关系是**静态的**（运维声明的 `peers` 表）；角色是**动态的**——选举只在这张表内部移动主节点。选举时序（`hb_interval` 200 ms、`down_after` 5 s、`election_timeout` 3 s）在本版本里是固定常量，不是配置键。

它驱动两个切主面：

**复制主节点选举（v3.15）。**当前主节点被判 DOWN 后，由一个有资格的副本发起竞选——存活 peer 中已应用复制 offset 最高的那个，平局时取 `node_id` 最小者——并需要在 `election_timeout` 内集齐多数派（`N/2 + 1`）的 ACCEPT。纪元与选票在任何应答发出*之前*就持久化到 `<data_dir>/elect.meta`，崩溃重启因此绝不会重复投票。胜者放开写入；败者自动把复制上游改指新主。三道钳制为此兜底：**冷启动**且没有已知主节点时，等满一个 `down_after` 宽限窗再选举；**重启**的多数派成员在选举落定前扣住写入（配置里的 `role = "primary"` 只是偏好）；看不到严格多数的主节点会在一个租约窗口内**围栏自己的写入**（`-NOREPLICAS primary lost quorum; writes fenced`）。运维层面的完整演练见 [`docs/availability.md`](availability.md)。

**作用域回退。**作用域声明的回退节点在每次接受写入时查一遍 DOWN 集合：若它对应的写者已 DOWN，回退节点就把自己视为生效持有者并接下这笔写；此后其他节点上的写都会 MISDIRECT 到回退节点。原写者的心跳恢复后，它退出 DOWN 集合，回退节点在下一次判定时隐式让位。

| 旋钮 | 含义 | 默认值 |
|------|------|--------|
| `node_id` | 本节点的稳定标识（≤ 32 B ASCII；作用域持有者与选举都引用它） | 必填 |
| `peers` | 集群全体成员的 `<node_id>@<host>:<elect_port>:<client_port>` 列表 | 必填 |
| `elect_port_base` | 选举控制面绑定的 TCP 端口（每节点一个监听） | `0` = 客户端端口 + 200 |

### 手工 rejoin 恢复

如果原写者 DOWN 得够久，久到回退节点已经接了写，那这些写只存在于回退节点上。重新让原写者接管该作用域之前：先停掉写者，把回退节点的数据目录拷贝到写者的数据目录，再重启。这仍在“无共识”契约之内——没有影子写入，也没有双重接受。

---

# `MOVE-SCOPE`

`MOVE-SCOPE` 在一个有界的静默窗口内，把一个前缀从一个写者迁到另一个写者。它由运维下发，在当前写者上执行。

```
MOVE-SCOPE <prefix> from <from-node-id> to <to-node-id>
```

流程如下：

1. 当前写者把 `<prefix>` 的本地状态翻成 MIGRATING。此后落在该前缀下的写返回 `-QUIESCED migrating to <to-host:port>`；客户端短暂退避后重试。
2. 写者把该前缀的键空间切片序列化，经 `MOVE-SCOPE-INGEST <prefix> <bulk>` 发到目标节点的数据端口。
3. 目标回 `+OK` 后，写者在本地提交这次迁移。源节点上此后对该前缀的写返回 `-MISDIRECTED writer is <to-host:port>`。
4. 集群其他成员继续按各自的静态 `scopes` 配置路由，直到运维推送新配置并重启它们。

迁移期间会看到两种线协议回复：

| 回复 | 含义 |
|------|------|
| `-MISDIRECTED writer is <host:port>` | 写落在了非持有者上。向指名的主机重试。 |
| `-QUIESCED migrating to <host:port>` | MOVE-SCOPE 窗口内的瞬时状态。退避后重试。 |

集群感知客户端收到 `-MISDIRECTED` 时缓存该键的新目标并透明重试；收到 `-QUIESCED` 时应先短暂 sleep（几百毫秒量级）再重试。

若迁移中途中止，归属回到源写者；目标上不会留下半应用状态。

---

# 配置参考

## 单节点集群模式

| TOML | CLI | Env | 默认 | 含义 |
|------|-----|-----|------|------|
| `[cluster] enabled` | `--cluster` | `KEVY_CLUSTER=1` | `false` | 把每个 shard 暴露在各自的端口上。 |
| `[cluster] port_base` | `--cluster-port-base` | `KEVY_CLUSTER_PORT_BASE` | `port` 的值 | shard `i` 绑定 `port_base + 1 + i`。 |

## 复制

只有 TOML——复制没有 CLI flag，也没有环境变量：

| TOML | 默认 | 含义 |
|------|------|------|
| `[replication] role` | `"standalone"` | `"primary"` 向副本流式发送；`"replica"` 从 `upstream` 拉取；`"standalone"` = 子系统休眠。 |
| `[replication] listen_port_base` | `0`（= `port` + 10000） | shard `i` 的复制端口绑定在 base + `i`；副本同样绑定（v3.15 提升对称性）。 |
| `[replication] upstream` | 未设置 | 仅副本：主节点复制端口基址的 `host:port`。 |

完整键列表（backlog 容量、`replica_read_only`、`replica_max_staleness_ms`、`min_replicas_to_write`、`single_source`）见 [`docs/replication.md`](replication.md)。

## 作用域多写者 + 选举

| TOML | 含义 |
|------|------|
| `[cluster] node_id` | 本节点的稳定标识（≤ 32 B ASCII）。 |
| `[cluster] peers` | 集群全体成员的 `<node_id>@<host>:<elect_port>:<client_port>` 列表（旧式两字段形式：两个端口取同一值）。 |
| `[cluster] scopes` | `prefix=writer[\|fallback]` 条目，逗号分隔。 |
| `[cluster] elect_port_base` | 选举控制面绑定的 TCP 端口；`0`（默认）= `port` + 200。 |

选举时序（心跳 200 ms、静默 5 s 判 DOWN、选举超时 3 s）是固定常量，不是配置键。

---

# 取舍与限制

- **单节点集群模式仍是单进程。**它换来的是客户端侧键路由，不是主机级容错。要容错，加副本。
- **代理端口一直可用。**非集群客户端继续用它，结果依旧正确，只是多付一次跨 shard 跳的开销。
- **成员关系静态，角色动态。**`peers` 和 `scopes` 在启动时从配置读入——变更成员靠“推新配置，重启”，刻意不做 gossip。在这张静态表内部，选举在运行时移动主节点角色（v3.15）；故障节点永远不会被自动顶替。
- **`MOVE-SCOPE` 会静默该前缀的写入。**窗口长度取决于切片传输时间，GB 级作用域走 LAN 是个位数秒。远大于此的前缀，请安排到维护窗口执行。
- **以 embed 作作用域写者是为服务形态的负载准备的**（账单服务、认证服务这一类），不适合多 TB 数据集。
- **回退节点接过写之后，rejoin 靠手工恢复。**重新启用写者前，先把回退节点的数据目录拷给它；没有自动的共识追赶。

---

# 设计上不在范围内

- AUTH 与 TLS——交给部署边缘（sidecar、mesh、LB），kevy 不做。
- 跨数据中心多活与 CRDT。
- 键空间之下的 Raft、Paxos 或任何共识日志。
- 基于 gossip 的发现—— `peers` 是静态的。
- 在线重新分片、`MIGRATE`、`ASK` 重定向。
- 归属重叠的多主——每个前缀在任一时刻恰好只有一个写者。

这些永远不会加。简单本身就是特性。

---

# FAQ

**用复制必须开集群模式吗？**
不必。单节点集群模式与复制/多节点层互相独立。非集群主节点可以带非集群副本；集群主节点也可以带集群副本。两者能组合，但互不依赖。

**标准的集群感知客户端（Lettuce、ioredis、redis-py-cluster）能连集群模式的 kevy 吗？**
能。`CLUSTER SLOTS / SHARDS / NODES` 通告的是真实分区，键打错 shard 时会返回 `-MOVED` ——这两样正是这些库依赖的全部接口面。记得连 per-shard 端口（而不是主代理端口），这样真正到达 shard 的才是客户端自己的路由。

**单节点集群模式下，跨 shard 的多键命令会怎样？**
会成功。kevy 在服务端执行跨 slot 的 `MGET`、`MSET`、`SUNION`、事务与阻塞扇出，而不是返回 `-CROSSSLOT`。对原子性敏感的场景，`{hashtag}` 共位仍然有用，但它不再是正确性要求。

**没有运维介入，写者崩溃怎么撑过去？**
复制主节点：在每个节点上配好 `[cluster]` 块（`node_id`、`elect_port_base`、`peers`）——主节点死亡会在 `down_after`（5 s）后检测到，进度最靠前的副本顶上；前主节点重新加入时会自动降级并重新同步（见 [`docs/availability.md`](availability.md)）。作用域写者：声明一个回退（`prefix=writer|fallback`）——写者心跳缺失超过 `down_after` 后，回退节点开始接受该前缀的写，客户端收到 `-MISDIRECTED writer is <fallback>` 后跟着走；原写者回来后，按手工 rejoin 恢复流程操作。

**为什么 gossip / Raft 永远不在范围内？**
给每一笔写都垫一层共识日志，其成本会抹掉让 kevy 值得一选的吞吐与尾延迟优势。静态配置 + 多数派心跳的设计让你拿到切主这条分支，又不必在热路径上为状态机复制买单。如果你的负载确实需要共识背书的键值存储，kevy 不是合适的工具。
