# 复制

kevy 如何把写入从主节点流式同步到一个或多个副本节点、如何手工或按多数派完成切主,以及一个嵌入式进程如何像只读副本一样订阅同一条流。

## 何时需要这份文档

当下面任一情况成立时请查阅复制:

- **读扇出。** 单个主节点承担所有写入;一个或多个副本承担读负载,并在 [`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) 客户端后面轮询。
- **高可用切换。** 你希望在当前主节点失联时,幸存的副本能自动选举出新主。加入 [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) 做基于多数派的提升;用 `FAILOVER` verb 做计划内零丢失交接;或用 `REPLICAOF NO ONE` 手工提升。
- **以 embed 作只读副本。** 应用使用 [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) 作为进程内键空间,但希望真相之源仍在一个 `kevy` 服务器上。Embed 在内存中镜像主节点,提供零网络往返的本地读;本地写入会被拒绝,必须发送到主节点。

如果你只跑一个 `kevy` 节点,你不需要这份文档。如果你需要跨数据中心多活、gossip 发现、在线 reshard、Raft、AUTH 或 TLS,kevy 永远不会提供这些 —— 请选择另一个系统。

## 核心思路

主 `kevy` 为每个 shard 打开一个专用的复制监听端口。每次施加的改动都被编码成一个 RESP 信封(`*2\r\n:<offset>\r\n<argv>`),带一个单调递增的 64-bit offset,并推入每个 shard 的有界环形 backlog。每个已连接的副本从它最后 ack 的 offset 流式拉取;如果请求的 offset 已经从 backlog 老化掉,主节点会就地内联推送一份该 shard 键空间的快照,然后无缝衔接到实时流。副本可以在运行时通过 `REPLICAOF host port` 切换上游,通过 `REPLICAOF NO ONE` 自降为独立节点。链式复制(副本之副本)在协议层不支持,并在 apply 路径上做了防御性拒绝。

从 v3.14 起这条连接是双向的。副本在施加帧的同时,通过同一条复制连接回写 `REPLCONF ACK <offset>`,因此主节点持有每个副本的 **acked** 位置(而不只是"已发送")—— `INFO replication` 的 `slave0` 行和 `WAIT` 栅栏读的就是它。反方向上,主节点以 1 Hz 追加一条带外心跳 `+PING <generation> <next_offset>` —— 它不占用 offset 空间,让副本得以自测滞后(`slave_lag_frames`)和链路存活(`master_link_status`)。`generation` 字段(v3.16)标识一段未曾断裂的 offset 历史;切主会将其递增。v3.16 之前的单数字 `+PING <next_offset>` 形式仍可解码。

从 v3.15 起拓扑是**对称的**:以 `role = "replica"` 启动的节点同样绑定完整的复制监听端口和复制源,因此一个被提升的副本可以立即服务下游副本 —— 不需要改配置,也不需要重启。

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

同一条复制流向三类订阅者投递:作为副本运行的完整 `kevy` 服务器、以副本模式开启的嵌入式 `kevy-embedded` `Store`,以及(间接地)用每个节点的 `repl_offset` 做切主决策的多数派选举器。

## 实际示例

下面的示例拉起一个主、一个副本,在运行时切换副本的上游,探查角色,并把一个进程内嵌入式 reader 挂到同一个主节点上。

### 1. 主节点 `kevy.toml`

```toml
[replication]
role             = "primary"
listen_port_base = 16004        # shard i binds replication on listen_port_base + i
replication_buffer_size = 268435456   # 256 MiB ring backlog per shard
reconnect_window_ms     = 60000       # how long to hold a slot for a reconnecting replica
```

启动:

```sh
kevy --config /etc/kevy/primary.toml --port 6004
```

主节点的 shard 0 现在在 `:6004` 接受 RESP 客户端流量,在 `:16004` 接受复制连接。

### 2. 副本节点 `kevy.toml`

```toml
[replication]
role     = "replica"
upstream = "primary.internal:16004"   # the primary's listen_port_base
```

在第二台主机上启动:

```sh
kevy --config /etc/kevy/replica.toml --port 6004
```

每个本地 shard 开一个 runner 线程,连接到 `(upstream_host, upstream_port_base + shard_index)`,以 `REPLICATE FROM <offset> ID <replica_id>` 握手,读取 `+ACK <offset>`,然后把帧流式写入 shard 的 apply 路径,过程中处于一个抑制本地重新发出的 guard 之内。

### 3. 在运行时切换副本上游

```sh
redis-cli -p 6004 REPLICAOF new-primary.internal 16004
# +OK
```

副本停止它的 runner 集群(socket 被关闭,以解除阻塞中的读),解析新的上游,然后生成新的 runner。本地 store **不会**被清空 —— 新主节点的帧会在已有数据上施加。如果你想要干净重放,请先 `FLUSHALL`。

### 4. 手工提升一个副本

```sh
redis-cli -p 6004 REPLICAOF NO ONE
# +OK
```

所有 runner 线程停止,生效角色翻转为 `master`。本地数据保持在最后施加的帧所在位置。以 `role = "replica"` 启动的节点已经绑定了完整的复制监听端口(拓扑对称性,v3.15),因此它在被提升的那一刻就能服务下游副本 —— 不需要改配置,也不需要重启。只有以 `standalone` 启动、又在运行时被切换上游的节点才缺少下游监听端口。

如果要做一次协调的、零丢失的交接(静默写入、等目标追平、提升、跟随),请改用 `FAILOVER` verb —— 见下文*计划内切主*。

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

两侧报告的都是心跳/ACK 的真值(v3.14):在主节点上,`slave0` 行的 `state` 在副本发出第一个 `REPLCONF ACK` 时从 `syncing` 翻为 `online`,`offset` 是它的 **acked** 位置,`lag` 以帧计;在副本上,只要 3 秒内有心跳落地,`master_link_status` 就是 `up`,`slave_lag_frames:0` 表示已追平。逐字段语义见 [`docs/availability.md`](availability.md) 的 *Observability* 一节。

`REPLICAOF` 设置的实时运行状态在回复里总是优先于静态配置 —— 而当配置了 elect 多数派时,实时选举角色又优先于两者。

### 6. 以 embed 作副本(一行)

应用可以通过 [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) 在进程内加入同一条复制流:

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

Embed 连到同一个 `listen_port_base` 对应的 shard,按到达顺序施加帧,并直接从本地 arena 提供读取。可运行示例在 [`crates/kevy-embedded/examples/replica.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/replica.rs)。

## 旋钮

服务器侧 TOML 在 `[replication]` 下:

| 键 | 默认值 | 含义 |
|---|---|---|
| `role` | `"standalone"` | `"standalone"` = 子系统休眠;`"primary"` 打开复制监听;`"replica"` 生成从 `upstream` 拉取的 runner。 |
| `listen_port_base` | `0`(= 客户端端口 + 10000)| Shard `i` 在 `listen_port_base + i` 上绑定复制端口。从 v3.15 起**副本也绑定这个监听端口**(提升对称性)。 |
| `upstream` | 未设置 | 仅副本。主节点复制端口基址的 `host:port`。每个本地 shard 连接 `(host, port + shard_index)`。 |
| `replication_buffer_size` | `268435456`(256 MiB)| 每 shard 环形 backlog 字节数。窗口内的重连走实时路径;更老的 offset 触发快照发送。 |
| `reconnect_window_ms` | `60000` | 主节点在回收某副本断开后保留它 offset slot 的时长。 |
| `replica_read_only` | `true` | 在副本上以 `-READONLY` 拒绝客户端写入;复制 apply 路径与管理类 verb 绕过此闸门。 |
| `replica_max_staleness_ms` | `0`(关)| 有界陈旧:副本最近一次收到主节点心跳的时间早于该界限时,以 `-STALE` 拒绝读取。[`docs/availability.md`](availability.md) 一致性阶梯第 3 级。 |
| `min_replicas_to_write` | `0`(关)| 健康副本(有活跃连接且已 ACK)少于 N 个时,主节点以 `-NOREPLICAS` 拒绝写入。阶梯第 4 级。 |
| `min_replicas_max_lag_ms` | `10000` | 为 `min_replicas_to_write` 预留的新鲜度窗口。 |
| `single_source` | `false` | 上游是单端口上的一条流(一个 embedded writer),而不是 per-shard 端口群 —— 见下文*以 embedded 作主节点*。 |

由于两种角色都会绑定复制端口段,在同一台机器上共同托管多个实例时,客户端端口之间至少要相隔 `nshards` —— 否则它们默认的复制端口段(`客户端端口 + 10000 … + 10000 + nshards − 1`)会冲突。

当配置了 [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) 时,`[cluster]` 块再加入多数派相关旋钮:

| 键 | 默认值 | 含义 |
|---|---|---|
| `node_id` | 未设置 | 本节点的稳定 id(≤ 32 B ASCII)。选举里作为 tie-breaker。 |
| `elect_port_base` | `0`(= 客户端端口 + 200)| 控制面 TCP 端口,用于心跳与选票 —— 每节点一个监听端口。 |
| `peers` | 空 | `id@host:elect_port:client_port,…`,集群里每个节点都写,包含自己。空表示选举器休眠。 |

请使用扩展的三字段 peer 语法:选举流量走 elect 端口,而切换上游与 `-MISDIRECTED` 回复用客户端端口。旧式 `id@host:port` 形式假定两者相等,这几乎从来不是你想要的。

选举时序是固定常量,不是配置键:心跳每 200 ms 一次,一个 peer 静默 5 s 后被判 DOWN,候选人等待多数派 ACCEPT 3 s。(本页早期版本曾把它们列为 `[cluster]` 键 —— 配置解析器会拒绝那些键。)

法定人数是 `N/2 + 1`。N=2 要求两个节点都在线(任何一个宕机都会让幸存者被锁成只读);linter 会警告,任何需要切换的部署都应使用 N ≥ 3。

需要提前规划的一个后果:在 elect 多数派中,`[replication] role = "primary"` 只是一个初始*偏好*。写权威来自赢得选举 —— 每个多数派成员都以只读状态启动并扣住写入,直到它胜选(冷启动也一样,第一次写入前要先付一轮选举)。正是这个钳制防止了一个重启后空空如也的主节点抹掉整个集群;完整叙述见 [`docs/availability.md`](availability.md) 的 *Election-only write authority* 一节。

## 切主

有两条路径可以移动主节点角色,都建立在上述流机制之上;操作细节(步骤、时序、错误契约)在 [`docs/availability.md`](availability.md)。

**计划内:`FAILOVER host port [TIMEOUT ms] | ABORT`**(v3.15)。在主节点上运行,参数是目标副本的*客户端*地址;它回答 `+OK`,并在后台线程完成交接:静默写入(`-QUIESCED`),轮询目标的 `INFO replication` 直到追平(`slave_lag_frames:0`),提升它(`REPLICAOF NO ONE`),然后作为副本跟随它。交接把上游切到 `客户端端口 + 10000`,因此目标必须使用默认的 `listen_port_base` 运行。超时(默认 10 000 ms)会回滚静默。

**崩溃:多数派选举**(v3.15)。在每个节点上配好 `[cluster]` 块之后,peer 们检测到主节点死亡,选出已施加复制 offset 最高的副本(`node_id` 最小者打破平局);胜者打开写入并递增自己的 feed generation,败者自动切换上游。一个重新加入的前主节点,如果它的流*领先*于新主(一段从未复制出去的分叉后缀),会得到一次**替换式**快照重同步 —— 加载前先 `FLUSHALL` —— 而不是被腐坏关闭:分叉被丢弃,该节点收敛到多数派的历史。

## 取舍与限制

复制**默认是异步的**。主节点在它知道任何副本是否已经施加该帧之前就先提交并回复;副本会落后于"一帧穿过网线并从 per-shard 通道汲取到 apply 路径"所需的时间。当某次写入或读取需要更强保证时,按调用购买:`WAIT n timeout` 阻塞到至少 n 个副本已确认,`REPL.TOKEN` + `REPL.WAIT` 在选定副本上给出读己之写,另有两个配置键提供有界陈旧(`-STALE`)与最少副本写闸门(`-NOREPLICAS`)。整个阶梯逐级见 [`docs/availability.md`](availability.md)。

| 关注点 | 答 |
|---|---|
| 写入耐久 | 主节点把帧落入本地 store 和 backlog 环之后就 ack。副本随后追上;`WAIT n timeout` 阻塞到至少 n 个副本已确认(副本 ack 不是 fsync —— 见 availability.md)。 |
| 读一致性 | 副本可能落后。通过 `kevy-cluster-rw` 发送 `request_read(…, consistent = true)` 把读强制走到主节点,或用 `REPL.TOKEN` + `REPL.WAIT` 在副本自身上实现读己之写。 |
| 副本掉队 | 如果重连请求的 offset 已从环里老化,主节点会就地内联推送一份该 shard 的快照,然后从快照末端的 offset 衔接实时帧 —— 没有 gap,无需人工介入。 |
| backlog 容量估算 | `replication_buffer_size ≈ peak_writes_per_sec × avg_argv_bytes × reconnect_window_seconds`。略大无害;过小会回退到快照发送。 |
| 切主后什么会变 | 写入会到新主,配置了 `kevy-elect` 时自动,否则手工。已有的 `kevy-cluster-rw` 客户端在学到新主后会把写入重路由;切换 gap 期间正在进行的写入会显式失败。 |
| 切主后什么不会变 | 跨数据中心流量、gossip 发现的 peer、在线 reshard、AUTH/TLS —— kevy 都不提供。仅限单数据中心。 |
| 链式复制 | 协议层不支持。副本的 apply 路径不会再向下游发出;配置错误会被防御性地拒绝。 |
| 分区少数派写入 | 有界,然后丢失。多数派集群中的主节点一旦看不到严格多数,会在一个租约窗口内围栏(fence)自己的写入(`-NOREPLICAS primary lost quorum; writes fenced`),因此静默吸收窗口约 5 s,窗口内的每次写入都会显式失败。分区内的少数派无法提升;分区恢复时它会自降,其未复制的分叉后缀被丢弃,并通过快照重同步到多数派的历史。 |

线协议(实时帧信封、快照发送、握手)记录在 [`crates/kevy-replicate/docs/wire.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/wire.md) 与 [`crates/kevy-replicate/docs/snapshot.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/snapshot.md)。选举协议见 [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md)。

## FAQ

**如何提升副本?**
计划内且零丢失:在当前主节点上运行 `FAILOVER host port`(见上文*切主*)。手工:连上副本运行 `REPLICAOF NO ONE` —— 生效角色立即翻为 `master`,本地 store 保留,开始接受写入,并且(从 v3.15 起)已经绑定的复制监听端口立刻开始服务下游副本。自动:在每个节点上配置带 `node_id`、`elect_port_base` 与 `peers` 列表的 `[cluster]`;已施加 offset 最高的在线副本在多数派下胜出。

**副本能晋升为主节点,然后再变回副本吗?**
可以。`REPLICAOF NO ONE` 只切断上游链接,不动数据;之后再 `REPLICAOF host port` 即可挂到新主。两次切换之间本地 store 都保留。如果你想从新上游做干净重放,先 `FLUSHALL`。

**数据丢失窗口有多大?**
就是"主节点 ack 客户端"与"每个副本都已施加该帧"之间的时间间隔。复制默认是异步的,所以一个在 ack 写入之后、在任何副本拿到帧之前崩溃的主节点会丢失这次写入。窗口大小取决于负载 —— 单数据中心 LAN 一般在亚毫秒级。对于必须在单节点损失中幸存的写入,跟一句 `WAIT 1 <timeout>`:一次被 `WAIT` 确认的写入存在于两个节点上,而崩溃选举会挑最领先的副本,因此它能幸存(见 [`docs/availability.md`](availability.md))。副本 ack 仍然不是 fsync;若需要跨断电也耐久,主节点上把复制配合 [`docs/persistence.md`](persistence.md)(AOF + RDB)一起使用。

**我能从副本读吗?**
能 —— 加副本的主要目的就是这个。使用 [`kevy-cluster-rw::ReadWriteClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw),它会把写发到主、按你传入的副本种子轮询读。当一次读必须看到最新写时,用同一个 client 的 consistent-read 路径强制这次读走主节点。

**有个副本落后太多了 —— 如何恢复?**
什么都不做。主节点发现副本请求的 offset 不在 backlog 环里时返回 `TooOld`,然后通过同一个 RESP 线连接就地内联推送一份该 shard 键空间的快照,再从快照末端的 offset 衔接实时帧。副本把快照换入,施加实时尾巴就追上了。如果你更想从空重建,停掉副本,删它的数据目录再重启 —— runner 会以 `from_offset = 0` 重新连接,并对整个键空间做一次快照发送。

## 参见

- [`docs/availability.md`](availability.md) —— 运维的那一半:拓扑、一致性阶梯、计划内 + 崩溃切主、错误契约。本页讲机制(线协议、帧、快照发送);那页讲该跑什么、客户端会看到什么。
- [`docs/cluster.md`](cluster.md) —— 多 shard 暴露与槽路由 `ClusterClient`;与复制正交,可组合。
- [`docs/persistence.md`](persistence.md) —— RDB 与 AOF;快照发送路径在线协议上复用同一份磁盘格式。
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) —— 读写分离 client。
- [`crates/kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) —— 多数派切换。
- [`crates/kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) —— `Store::open_replica` 以 embed 作副本。

## 以 embedded 作主节点(v3.2)

嵌入式应用可以充当 PRIMARY,让一个 kevy 服务器作它的副本 —— 为进程内 store 提供读扩展和完整查询面(副本在复制过来的数据上声明自己的索引/视图/聚合):

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

`single_source = true` 告诉服务器:上游是一条单一的流(embed writer 源),而不是服务器↔服务器复制的 per-shard 端口群:一个 runner 连接上游,带键的帧按 key hash 路由到本地 shard,FLUSHALL/FLUSHDB 广播,快照发送则广播整个负载、每个 shard 只加载属于自己 hash 切片的部分。

从 offset 0(全新)或从超出 backlog 窗口处握手的副本,会从 embed 源收到一次完整快照发送(v1.21 的反 scope,在 v3.2 关闭):对每个 shard 做一次时间点冻结,附带 as-of offset,然后衔接实时帧。

与 CDC feed([docs/cdc.md](../cdc.md))的关系:两者按设计共存。复制源服务的是副本一致性(基础设施平面,per-source offset);feed 服务的是应用级 CDC((generation, offset) 游标、前缀过滤、at-least-once)。把它们统一起来会把面向应用的 CDC 语义绑死在副本协议上。

Gate:`bench/repligate.sh` —— 真双进程:对全新副本做快照发送、静默后 digest 稳定性、重启后重同步,以及副本本地在复制数据之上的 `IDX.CREATE`/`IDX.QUERY`。
