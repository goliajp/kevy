# 复制

本文讲 kevy 如何把写入从主节点流式同步到一个或多个副本、如何手工或按多数派完成切主，以及嵌入式进程如何以只读副本的身份订阅同一条流。

## 什么时候需要这份文档

下面任一情况成立时，就该看复制：

- **读扇出。**单个主节点承担全部写入；一个或多个副本分担读负载，在 [`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) 客户端后面轮询。
- **高可用切主。**希望当前主节点失联时，幸存的副本自动选出新主。加入 [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) 做基于多数派的提升；用 `FAILOVER` verb 做计划内零丢失交接；或者用 `REPLICAOF NO ONE` 手工提升。
- **以 embed 作只读副本。**应用用 [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) 做进程内键空间，但希望真相之源留在某台 `kevy` 服务器上。Embed 在内存中镜像主节点，本地读零网络往返；本地写会被拒绝，必须发给主节点。

只跑一个 `kevy` 节点的话，用不着这份文档。如果你要的是跨数据中心多活、gossip 发现、在线 reshard、Raft、AUTH 或 TLS——kevy 永远不会提供，请换一个系统。

## 核心思路

主节点为每个 shard 打开一个专用的复制监听端口。每一笔已应用的改动都编码成一个 RESP 信封（`*2\r\n:<offset>\r\n<argv>`），携带单调递增的 64-bit offset，推入该 shard 的有界环形 backlog。每个已连接的副本从自己最后 ack 的 offset 开始流式拉取；如果请求的 offset 已经从 backlog 里老化掉，主节点就在同一条连接上内联推送一份该 shard 键空间的快照，然后无缝衔接实时流。副本可以在运行时用 `REPLICAOF host port` 切换上游，用 `REPLICAOF NO ONE` 自降为独立节点。链式复制（副本之副本）在协议层不支持，apply 路径上也做了防御性拒绝。

从 v3.14 起这条连接是双向的。副本一边应用帧，一边在同一条复制连接上回写 `REPLCONF ACK <offset>`，所以主节点掌握每个副本的 **acked** 位置（而不只是“已发送”）——`INFO replication` 的 `slave0` 行和 `WAIT` 屏障读的就是它。反方向上，主节点每秒追加一条带外心跳 `+PING <generation> <next_offset>`——它不占用 offset 空间，让副本能自测滞后（`slave_lag_frames`）和链路存活（`master_link_status`）。`generation` 字段（v3.16）标识一段从未断裂的 offset 历史；切主会递增它。v3.16 之前的单数字形式 `+PING <next_offset>` 仍然可以解码。

从 v3.15 起拓扑是**对称的**：以 `role = "replica"` 启动的节点同样绑定完整的复制监听端口和复制源，所以副本一旦提升，立刻就能服务下游副本——不改配置，不用重启。

```
                  +-----------------+
   writes ──────► |    primary      |
                  |  shard 0..N-1   |
                  |  port_base + i  |
                  +--------+--------+
                           │ per-shard RESP stream (offset, argv)
            ┌──────────────┼──────────────┐
            ▼              ▼              ▼
       +---------+    +---------+    +---------+
       | replica |    | replica |    | embed   |
       |   A     |    |   B     |    | (in-proc|
       |  reads  |    |  reads  |    |  reader)|
       +---------+    +---------+    +---------+
```

同一条复制流喂给三类订阅者：作为副本运行的完整 `kevy` 服务器、以副本模式打开的嵌入式 `kevy-embedded` `Store`，以及（间接地）盯着每个节点 `repl_offset` 做切主决策的多数派选举器。

## 实例

下面的示例拉起一主一副本，在运行时切换副本的上游，探查角色，最后把一个进程内嵌入式 reader 挂到同一个主节点上。

### 1. 主节点 `kevy.toml`

```toml
[replication]
role             = "primary"
listen_port_base = 16004        # shard i binds replication on listen_port_base + i
replication_buffer_size = 268435456   # 256 MiB ring backlog per shard
reconnect_window_ms     = 60000       # how long to hold a slot for a reconnecting replica
```

启动：

```sh
kevy --config /etc/kevy/primary.toml --port 6004
```

主节点的 shard 0 现在在 `:6004` 接受 RESP 客户端流量，在 `:16004` 接受复制连接。

### 2. 副本节点 `kevy.toml`

```toml
[replication]
role     = "replica"
upstream = "primary.internal:16004"   # the primary's listen_port_base
```

在第二台主机上启动：

```sh
kevy --config /etc/kevy/replica.toml --port 6004
```

每个本地 shard 开一个 runner 线程，连接 `(upstream_host, upstream_port_base + shard_index)`，以 `REPLICATE FROM <offset> ID <replica_id>` 握手，读到 `+ACK <offset>` 后，把帧流式写入 shard 的 apply 路径——整个过程处在一个抑制本地重新发出的 guard 之内。

### 3. 在运行时切换副本上游

```sh
redis-cli -p 6004 REPLICAOF new-primary.internal 16004
# +OK
```

副本停掉自己的 runner 集群（关闭 socket，让阻塞中的读解除），解析新上游，再生成新的 runner。本地 store **不会**清空——新主节点的帧直接落在已有数据之上。想要干净重放，先执行 `FLUSHALL`。

### 4. 手工提升副本

```sh
redis-cli -p 6004 REPLICAOF NO ONE
# +OK
```

所有 runner 线程停止，生效角色翻转为 `master`。本地数据停在最后一帧应用完的位置。以 `role = "replica"` 启动的节点本来就绑定了完整的复制监听端口（拓扑对称性，v3.15），所以提升的那一刻就能服务下游副本——不改配置，不用重启。只有以 `standalone` 启动、又在运行时被切换上游的节点，才缺下游监听端口。

要做一次协调的零丢失交接（静默写入、等目标追平、提升、跟随），请改用 `FAILOVER` verb——见下文“切主”。

### 5. 探查角色

```sh
redis-cli -p 6004 ROLE
# 1) "master"
# 2) (integer) 12345678
# 3) 1) 1) "10.0.0.21"
#       2) (integer) 6004
#       3) (integer) 12345670

redis-cli -p 6004 INFO replication          # on the primary
# role:master
# connected_slaves:1
# slave0:ip=10.0.0.21,port=6004,state=online,offset=12345670,sent=12345678,lag=8
# master_repl_offset:12345678

redis-cli -p 6004 INFO replication          # on the replica
# role:slave
# master_host:primary.internal
# master_port:16004
# master_link_status:up
# master_last_io_seconds_ago:0
# slave_read_only:1
# slave_repl_offset:12345670
# slave_lag_frames:0
```

两侧报告的都是心跳/ACK 的真值（v3.14）：主节点上，`slave0` 行的 `state` 在每条 shard 流都收到真实 `REPLCONF ACK` 后由 `syncing` 翻为 `online`，`offset` 是它的 **acked** 位置，`lag` 以帧计；副本上，只要 3 秒内有心跳落地，`master_link_status` 就是 `up`，`slave_lag_frames:0` 表示已追平。`ROLE` 与 `INFO replication` 跨 shard 聚合：offset 是各 shard 流之和，且无论 shard 数多少，每个副本进程恰好对应一条 `slaveN` 记录。逐字段语义见 [`docs/availability.md`](availability.md) 的“可观测性”一节。

回复中，`REPLICAOF` 设置的实时运行状态永远优先于静态配置——配置了 elect 多数派时，实时选举角色又优先于两者。

### 6. 以 embed 作副本（一行代码）

应用可以通过 [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) 在进程内加入同一条复制流：

```rust
use kevy_embedded::Store;

let store = Store::open_replica("primary.internal:16004")?;
assert!(store.is_replica());

// Local writes are rejected with READONLY.
assert!(store.set(b"local", b"nope").is_err());

// Reads pay zero network round-trip — the keyspace lives in this process.
if let Some(v) = store.get(b"hello")? {
    println!("{:?}", v);
}
```

Embed 连到同一个 `listen_port_base` 对应的 shard，帧到即应用，读取直接走本地 arena。可运行的完整示例在 [`crates/kevy-embedded/examples/replica.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/replica.rs)。

## 旋钮

服务器侧 TOML，`[replication]` 下：

| 键 | 默认值 | 含义 |
|---|---|---|
| `role` | `"standalone"` | `"standalone"` = 子系统休眠；`"primary"` 打开复制监听；`"replica"` 生成从 `upstream` 拉取的 runner。 |
| `listen_port_base` | `0`（= 客户端端口 + 10000） | shard `i` 在 `listen_port_base + i` 上绑定复制端口。从 v3.15 起**副本也绑定这个监听端口**（提升对称性）。 |
| `upstream` | 未设置 | 仅副本用。主节点复制端口基址的 `host:port`。每个本地 shard 连接 `(host, port + shard_index)`。 |
| `replication_buffer_size` | `268435456`（256 MiB） | 每 shard 的环形 backlog 字节数。窗口内的重连走实时路径；更老的 offset 触发快照发送。 |
| `reconnect_window_ms` | `60000` | 副本掉线后，主节点为它的 offset 槽位保留多久再回收。 |
| `replica_read_only` | `true` | 副本上以 `-READONLY` 拒绝客户端写；复制 apply 路径和管理类 verb 不受此闸门约束。 |
| `replica_max_staleness_ms` | `0`（关） | 有界陈旧：副本上一次收到主节点心跳的时间超过界限时，以 `-STALE` 拒绝读。对应 [`docs/availability.md`](availability.md) 一致性阶梯第 3 级。 |
| `min_replicas_to_write` | `0`（关） | 健康副本（有活跃连接且已 ACK）不足 N 个时，主节点以 `-NOREPLICAS` 拒绝写。阶梯第 4 级。 |
| `min_replicas_max_lag_ms` | `10000` | `min_replicas_to_write` 的新鲜度窗口：副本只有在最近一次 ACK 比这个界限新时才算健康——连接还挂着但已停摆的副本会从计数里老化出去。 |
| `single_source` | `false` | 上游是单端口上的一条流（embedded writer），不走 per-shard 端口群——见下文“以 embedded 作主节点”。 |

因为两种角色都会绑定复制端口段，同一台机器上共同托管多个实例时，客户端端口之间至少要间隔 `nshards`——否则它们默认的复制端口段（客户端端口 + 10000 … + 10000 + nshards − 1）会撞在一起。

配置了 [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) 时，`[cluster]` 块再提供多数派相关的旋钮：

| 键 | 默认值 | 含义 |
|---|---|---|
| `node_id` | 未设置 | 本节点的稳定 id（≤ 32 B ASCII）。选举中作平局裁决。 |
| `elect_port_base` | `0`（= 客户端端口 + 200） | 控制面 TCP 端口，承载心跳与选票——每节点一个监听。 |
| `peers` | 空 | `id@host:elect_port:client_port,…`，集群里每个节点都要列出，包括自己。留空则选举器休眠。 |

peer 请写成扩展的三字段语法：选举流量走 elect 端口，切换上游和 `-MISDIRECTED` 回复用客户端端口。旧式 `id@host:port` 写法假定两者相等，这基本不会是你要的效果。

选举时序是固定常量，不是配置键：心跳每 200 ms 一次，peer 静默 5 s 判 DOWN，候选人等待多数派 ACCEPT 3 s。（本页早期版本曾把它们列成 `[cluster]` 键——配置解析器会拒绝那些键。）

多数派规模是 `N/2 + 1`。N=2 要求两个节点都在线（任意一台宕机都会把幸存者锁成只读）；linter 会对此告警，凡是需要切主的部署都应使用 N ≥ 3。

有一个后果要提前规划：在 elect 多数派里，`[replication] role = "primary"` 只是初始“偏好”。写权限来自赢得选举——每个多数派成员都以只读状态启动、扣住写入直到胜选（冷启动也一样，第一笔写之前要先付一轮选举）。正是这道钳制，防止了重启后空空如也的主节点抹掉整个集群；完整叙述见 [`docs/availability.md`](availability.md) 的“写权限只来自选举”一节。

## 切主

移动主节点角色有两条路径，都建立在上述流机制之上；操作细节（步骤、时序、错误契约）在 [`docs/availability.md`](availability.md)。

**计划内：`FAILOVER host port [TIMEOUT ms] | ABORT`**（v3.15）。在主节点上执行，参数是目标副本的**客户端**地址；它回答 `+OK`，交接在后台线程完成：静默写入（`-QUIESCED`），轮询目标的 `INFO replication` 直到追平（`slave_lag_frames:0`），提升目标（`REPLICAOF NO ONE`），然后自己作为副本跟随过去。交接会把上游切到“客户端端口 + 10000”，所以目标必须用默认的 `listen_port_base` 运行。超时（默认 10000 ms）会回滚静默。

**崩溃：多数派选举**（v3.15）。每个节点都配好 `[cluster]` 块之后，各 peer 检测到主节点死亡，选出已应用复制 offset 最高的副本（平局时 `node_id` 最小者胜出）；胜者打开写入并递增自己的 feed generation，败者自动切换上游。重新加入的前主节点如果流位置**领先**于新主（一段从未复制出去的分叉后缀），会得到一次**替换式**快照重同步——加载前先 `FLUSHALL`——而不是被判定损坏后关闭：分叉丢弃，节点收敛到多数派的历史。

## 取舍与限制

复制**默认异步**。主节点在得知任何副本是否应用了帧之前就提交并回复；副本落后的时间，等于一帧穿过网线、再从 per-shard 通道流入 apply 路径的耗时。哪次写入或读取需要更强保证，就按调用单独购买：`WAIT n timeout` 阻塞到至少 n 个副本确认，`REPL.TOKEN` + `REPL.WAIT` 在选定副本上给出读己之写，另有两个配置键提供有界陈旧（`-STALE`）和最少副本写闸门（`-NOREPLICAS`）。整个阶梯逐级讲解见 [`docs/availability.md`](availability.md)。

| 关注点 | 回答 |
|---|---|
| 写入耐久性 | 帧落进本地 store 和 backlog 环之后主节点就 ack。副本随后追赶；`WAIT n timeout` 阻塞到至少 n 个副本确认（副本 ack 不是 fsync——见 availability.md）。 |
| 读一致性 | 副本可能滞后。通过 `kevy-cluster-rw` 发 `request_read(…, consistent = true)` 把读强制走主节点，或用 `REPL.TOKEN` + `REPL.WAIT` 在副本上实现读己之写。 |
| 副本掉队 | 重连请求的 offset 已从环里老化时，主节点就地内联推送一份该 shard 的快照，再从快照末端 offset 衔接实时帧——没有空洞，无需人工介入。快照替换副本键空间期间，副本上的客户端读回答 `-LOADING`（`PING` / `INFO` / `HELLO` 照常应答，健康检查不受影响）。 |
| backlog 容量估算 | `replication_buffer_size ≈ peak_writes_per_sec × avg_argv_bytes × reconnect_window_seconds`。偏大无害；偏小会退化成快照发送。 |
| 切主后什么会变 | 写入改发新主节点——配了 `kevy-elect` 就自动，否则手工。已有的 `kevy-cluster-rw` 客户端得知新主后自动改道；切换空档内正在进行的写会显式失败。 |
| 切主后什么不会变 | 跨数据中心流量、gossip 发现的 peer、在线 reshard、AUTH/TLS——kevy 一概不提供。仅限单数据中心。 |
| 链式复制 | 协议层不支持。副本的 apply 路径不会再向下游发出；这种误配置会被防御性拒绝。 |
| 分区期间少数派的写入 | 有界，然后丢失。多数派集群中的主节点一旦看不到严格多数，会在一个租约窗口内围栏自己的写入（`-NOREPLICAS primary lost quorum; writes fenced`），所以静默吸收窗口约 5 s，且窗口内每笔写都显式失败。分区里的少数派无法提升；分区愈合时它自降，未复制出去的分叉后缀丢弃，通过快照重同步到多数派的历史。 |

线协议（实时帧信封、快照发送、握手）记录在 [`crates/kevy-replicate/docs/wire.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/wire.md) 和 [`crates/kevy-replicate/docs/snapshot.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/snapshot.md)。选举协议见 [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md)。

## FAQ

**如何提升副本？**
计划内、零丢失：在当前主节点上执行 `FAILOVER host port`（见上文“切主”）。手工：连上副本执行 `REPLICAOF NO ONE`——生效角色立即翻为 `master`，本地 store 保留，开始接受写入，而且（v3.15 起）早已绑定的复制监听端口立刻开始服务下游副本。自动：在每个节点上配置带 `node_id`、`elect_port_base` 和 `peers` 列表的 `[cluster]`；在线副本中已应用 offset 最高者按多数派胜出。

**副本升成主节点之后，还能再变回副本吗？**
能。`REPLICAOF NO ONE` 只切断上游链接，不动数据；之后再 `REPLICAOF host port` 就能挂到新主节点上。两次切换之间本地 store 都保留。想从新上游干净重放，先 `FLUSHALL`。

**数据丢失窗口有多大？**
就是“主节点 ack 客户端”到“每个副本都应用了这一帧”之间的间隔。复制默认异步，主节点若在 ack 之后、副本拿到帧之前崩溃，这笔写就丢了。窗口大小取决于负载——单数据中心 LAN 上通常在亚毫秒级。必须扛住单节点损失的写入，后面跟一句 `WAIT 1 <timeout>`：被 `WAIT` 确认的写入存在于两个节点上，而崩溃选举会挑最领先的副本，所以它能幸存（见 [`docs/availability.md`](availability.md)）。副本 ack 依然不是 fsync；要跨断电耐久，主节点上把复制和 [`docs/persistence.md`](persistence.md)（AOF + RDB）搭配使用。

**能从副本读吗？**
能——加副本的主要目的就是这个。用 [`kevy-cluster-rw::ReadWriteClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw)，它把写发到主节点，读在你传入的副本种子间轮询。哪次读必须看到最新写入，就用同一个客户端的 consistent-read 路径，把那次读强制走主节点。

**有个副本落后太多——怎么恢复？**
什么都不用做。主节点发现副本请求的 offset 不在 backlog 环里，返回 `TooOld`，然后通过同一条 RESP 连接就地内联推送一份该 shard 键空间的快照，再从快照末端 offset 衔接实时帧。副本换入快照、应用实时尾巴，就追平了。快照传送窗口内，副本以 `-LOADING` 拒绝客户端读（可见键空间即将被整体替换）；`PING`、`INFO`、`HELLO` 豁免，且 `INFO replication` 报告 `loading:1`，监控可以盯着窗口收拢。要是宁可从零重建：停掉副本，删数据目录，重启——runner 会以 `from_offset = 0` 重连，对整个键空间做一次快照发送。

## 参见

- [`docs/availability.md`](availability.md)——运维的那一半：拓扑、一致性阶梯、计划内 + 崩溃切主、错误契约。本页讲机制（线协议、帧、快照发送）；那页讲该跑什么、客户端会看到什么。
- [`docs/cluster.md`](cluster.md)——多 shard 暴露与槽路由的 `ClusterClient`；与复制正交，可组合。
- [`docs/persistence.md`](persistence.md)——RDB 与 AOF；快照发送路径在线协议上复用同一份磁盘格式。
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw)——读写分离客户端。
- [`crates/kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect)——多数派切主。
- [`crates/kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded)——以 embed 作副本的 `Store::open_replica`。

## 以 embedded 作主节点（v3.2）

嵌入式应用也可以反过来当 PRIMARY，让一台 kevy 服务器作它的副本——给进程内 store 换来读扩展和完整的查询面（副本可以在复制过来的数据上声明自己的索引/视图/聚合）：

```rust
// the application (primary)
let store = Store::open(
    Config::default().with_shards(4).with_embed_writer("127.0.0.1:7101"),
)?;
```

```toml
# the server replica (replica.toml)
[replication]
role = "replica"
upstream = "127.0.0.1:7101"
single_source = true          # ONE upstream stream, hash-routed locally
```

`single_source = true` 告诉服务器：上游是单独一条流（embed writer 源），不是服务器↔服务器复制的 per-shard 端口群。一个 runner 连上游，带键的帧按 key hash 路由到本地 shard，FLUSHALL/FLUSHDB 广播，快照发送则广播整个负载、每个 shard 只加载属于自己 hash 切片的那部分。

从 offset 0（全新）或超出 backlog 窗口处握手的副本，会从 embed 源收到一次完整快照发送（v1.21 的反 scope，在 v3.2 关闭）：对每个 shard 做时间点冻结，附带 as-of offset，然后衔接实时帧。

与 CDC feed（[docs/cdc.md](../cdc.md)）的关系：两者按设计共存。复制源服务的是副本一致性（基础设施平面，per-source offset）；feed 服务的是应用级 CDC（`(generation, offset)` 游标、前缀过滤、at-least-once）。硬把两者统一，会把面向应用的 CDC 语义绑死在副本协议上。

Gate：`bench/repligate.sh`——真双进程验证：对全新副本的快照发送、静默后 digest 稳定性、重启后重同步，以及副本本地在复制数据之上的 `IDX.CREATE`/`IDX.QUERY`。
