# 复制

kevy 如何把写入从主节点流式同步到一个或多个副本节点、如何手工或按多数票完成切主,以及一个嵌入式进程如何像只读副本一样订阅同一条流。

## 何时需要这份文档

当下面任一情况成立时请查阅复制:

- **读扇出。** 单个主节点承担所有写入;一个或多个副本承担读负载,并在 [`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) 客户端后面轮询。
- **高可用切换。** 你希望在当前主节点失联时,幸存的副本能自动选举出新主。加入 [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) 做基于多数派的提升;否则用 `REPLICAOF NO ONE` 手工切换。
- **以 embed 作只读副本。** 应用使用 [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) 作为进程内键空间,但希望真相之源仍在一个 `kevy` 服务器上。Embed 在内存中镜像主节点,提供零网络往返的本地读;本地写入会被拒绝,必须发送到主节点。

如果你只跑一个 `kevy` 节点,你不需要这份文档。如果你需要跨数据中心多活、gossip 发现、在线 reshard、Raft、AUTH 或 TLS,kevy 永远不会提供这些 —— 请选择另一个系统。

## 核心思路

主 `kevy` 为每个 shard 打开一个专用的复制监听端口。每次施加的改动都被编码成一个 RESP 信封(`*2\r\n:<offset>\r\n<argv>`),带一个单调递增的 64-bit offset,并推入每个 shard 的有界环形 backlog。每个已连接的副本从它最后 ack 的 offset 流式拉取;如果请求的 offset 已经从 backlog 老化掉,主节点会就地内联推送一份该 shard 键空间的快照,然后无缝衔接到实时流。副本可以在运行时通过 `REPLICAOF host port` 切换上游,通过 `REPLICAOF NO ONE` 自降为独立节点。链式复制(副本之副本)在协议层不支持,并在 apply 路径上做了防御性拒绝。

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
listen_port_base = 16004        # shard i 在 listen_port_base + i 上绑定复制端口
replication_buffer_size = 268435456   # 每 shard 256 MiB 的环形 backlog
reconnect_window_ms     = 60000       # 为重连副本保留 slot 的窗口
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
upstream = "primary.internal:16004"   # 主节点的 listen_port_base
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

所有 runner 线程停止,生效角色翻转为 `master`。本地数据保持在最后施加的帧所在位置。要接受下游副本,你还得编辑配置(`role = "primary"` + `listen_port_base`)并重启 —— 运行时的 `REPLICAOF NO ONE` 不会绑定下游监听端口。

### 5. 探查角色

```sh
redis-cli -p 6004 ROLE
# 1) "master"
# 2) (integer) 12345678
# 3) 1) 1) "10.0.0.21"
#       2) (integer) 6004
#       3) (integer) 12345670

redis-cli -p 6004 INFO replication
# role:master
# connected_slaves:1
# master_repl_offset:12345678
# slave0:ip=10.0.0.21,port=6004,offset=12345670
```

回复里总是 `REPLICAOF` 设置的实时运行状态优先,而不是静态配置。

### 6. 以 embed 作副本(一行)

应用可以通过 [`kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) 在进程内加入同一条复制流:

```rust
use kevy_embedded::Store;

let store = Store::open_replica("primary.internal:16004")?;
assert!(store.is_replica());

// 本地写入被以 READONLY 拒绝。
assert!(store.set(b"local", b"nope").is_err());

// 读取零网络往返 —— 键空间就活在这个进程里。
if let Some(v) = store.get(b"hello")? {
    println!("{:?}", v);
}
```

Embed 连到同一个 `listen_port_base` 对应的 shard,按到达顺序施加帧,并直接从本地 arena 提供读取。可运行示例在 [`crates/kevy-embedded/examples/replica.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded/examples/replica.rs)。

## 旋钮

服务器侧 TOML 在 `[replication]` 下:

| 键 | 默认值 | 含义 |
|---|---|---|
| `role` | `"primary"` | `"primary"` 打开复制监听;`"replica"` 生成从 `upstream` 拉取的 runner。 |
| `listen_port_base` | `16004`(主)| 主节点的 shard `i` 在 `listen_port_base + i` 上绑定复制端口。副本连同样的偏移。 |
| `upstream` | 未设置 | 仅副本。主节点 `listen_port_base` 的 `host:port`。每个本地 shard 连接 `(host, port + shard_index)`。 |
| `replication_buffer_size` | `268435456`(256 MiB)| 每 shard 环形 backlog 字节数。窗口内的重连走实时路径;更老的 offset 触发快照发送。 |
| `reconnect_window_ms` | `60000` | 主节点在回收某副本断开后保留它 offset slot 的时长。 |

当配置了 [`kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) 时,`[cluster]` 块再加入多数派相关旋钮:

| 键 | 默认值 | 含义 |
|---|---|---|
| `node_id` | 未设置 | 本节点的稳定 id(≤ 32 B ASCII)。选举里作为 tie-breaker。 |
| `elect_port_base` | 未设置 | 控制面 TCP 端口,用于心跳与选票。shard 0 绑在 `elect_port_base + 0`。 |
| `peers` | 空 | `id@host:port,…`,集群里每个节点都写,包含自己。空表示选举器休眠。 |
| `hb_interval_ms` | `200` | 对每个 peer 发出心跳的周期。 |
| `down_after_ms` | `5000` | 一个 peer 在这么多毫秒没有心跳后被标记 DOWN。 |
| `election_timeout_ms` | `3000` | 候选人等待多数派 `ACCEPT` 的时长。 |

法定人数是 `N/2 + 1`。N=2 要求两个节点都在线(任何一个宕机都会让幸存者被锁成只读);linter 会警告,任何需要切换的部署都应使用 N ≥ 3。

## 取舍与限制

复制是**异步**的。主节点在它知道任何副本是否已经施加该帧之前就先提交并回复;副本会落后于"一帧穿过网线并从 per-shard 通道汲取到 apply 路径"所需的时间。没有 `WAIT` 风格的栅栏,也没有同步模式。

| 关注点 | 答 |
|---|---|
| 写入耐久 | 主节点把帧落入本地 store 和 backlog 环之后就 ack。副本随后追上。 |
| 读一致性 | 副本可能落后。通过 `kevy-cluster-rw` 发送 `request_read(…, consistent = true)`,在需要 read-after-write 时把读强制走到主节点。 |
| 副本掉队 | 如果重连请求的 offset 已从环里老化,主节点会就地内联推送一份该 shard 的快照,然后从快照末端的 offset 衔接实时帧 —— 没有 gap,无需人工介入。 |
| backlog 容量估算 | `replication_buffer_size ≈ peak_writes_per_sec × avg_argv_bytes × reconnect_window_seconds`。略大无害;过小会回退到快照发送。 |
| 切主后什么会变 | 写入会到新主,配置了 `kevy-elect` 时自动,否则手工。已有的 `kevy-cluster-rw` 客户端在学到新主后会把写入重路由;切换 gap 期间正在进行的写入会显式失败。 |
| 切主后什么不会变 | 跨数据中心流量、gossip 发现的 peer、在线 reshard、AUTH/TLS —— kevy 都不提供。仅限单数据中心。 |
| 链式复制 | 协议层不支持。副本的 apply 路径不会再向下游发出;配置错误会被防御性地拒绝。 |
| 分区少数派写入 | 丢失。分区内的少数派无法达成多数派,无法提升;分区恢复时它会自降并接受多数派的历史。在写入侧使用 consistent-read 路径以避免脏读。 |

线协议(实时帧信封、快照发送、握手)记录在 [`crates/kevy-replicate/docs/wire.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/wire.md) 与 [`crates/kevy-replicate/docs/snapshot.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-replicate/docs/snapshot.md)。选举协议见 [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md)。

## FAQ

**如何提升副本?**
手工:连上副本运行 `REPLICAOF NO ONE`。生效角色立即翻为 `master`,本地 store 保留,并开始接受写入。要接受下游副本,还要在 TOML 里更新 `role` 与 `listen_port_base` 并重启。自动:在每个节点上配置带 `node_id`、`elect_port_base` 与 `peers` 列表的 `kevy-elect`;`repl_offset` 最高的在线副本在多数派下胜出。

**副本能晋升为主节点,然后再变回副本吗?**
可以。`REPLICAOF NO ONE` 只切断上游链接,不动数据;之后再 `REPLICAOF host port` 即可挂到新主。两次切换之间本地 store 都保留。如果你想从新上游做干净重放,先 `FLUSHALL`。

**数据丢失窗口有多大?**
就是"主节点 ack 客户端"与"每个副本都已施加该帧"之间的时间间隔。复制是异步的,所以一个在 ack 写入之后、在任何副本拿到帧之前崩溃的主节点会丢失这次写入。窗口大小取决于负载 —— 单数据中心 LAN 一般在亚毫秒级。没有同步模式;若需要跨断电也耐久,主节点上把复制配合 [`docs/persistence.md`](persistence.md) (AOF + RDB) 一起使用。

**我能从副本读吗?**
能 —— 加副本的主要目的就是这个。使用 [`kevy-cluster-rw::ReadWriteClient`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw),它会把写发到主、按你传入的副本种子轮询读。当一次读必须看到最新写时,用同一个 client 的 consistent-read 路径强制这次读走主节点。

**有个副本落后太多了 —— 如何恢复?**
什么都不做。主节点发现副本请求的 offset 不在 backlog 环里时返回 `TooOld`,然后通过同一个 RESP 线连接就地内联推送一份该 shard 键空间的快照,再从快照末端的 offset 衔接实时帧。副本把快照换入,施加实时尾巴就追上了。如果你更想从空重建,停掉副本,删它的数据目录再重启 —— runner 会以 `from_offset = 0` 重新连接,并对整个键空间做一次快照发送。

## 参见

- [`docs/cluster.md`](cluster.md) —— 多 shard 暴露与槽路由 `ClusterClient`;与复制正交,可组合。
- [`docs/persistence.md`](persistence.md) —— RDB 与 AOF;快照发送路径在线协议上复用同一份磁盘格式。
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) —— 读写分离 client。
- [`crates/kevy-elect`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect) —— 多数派切换。
- [`crates/kevy-embedded`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-embedded) —— `Store::open_replica` 以 embed 作副本。
