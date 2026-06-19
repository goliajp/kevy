# 集群模式 + 集群感知客户端(kevy-client 1.9.0)

kevy 是 **单节点、多 shard** 引擎。集群模式不是多主机分布(没有
failover、gossip、在线 resharding、MIGRATE/ASK —— 它们永久不在范围
里)。它是把每个内部 shard 暴露成可寻址 cluster 节点的方式,让 key
感知客户端**直接跟拥有 key 的 shard 对话**,跳过 server 端跨 shard 转
发跳转。

那一跳是核心问题:默认单端口代理行为下,落到错 shard 的命令在内部转
发给 owner。那次转发是正确的,但有代价 —— 低负载时支配尾延迟,高负
载时支配吞吐(实测:见下方[性能](#性能))。集群模式 + 路由客户端去掉它。

## 服务器端 —— `--cluster`

```sh
kevy --threads 8 --cluster          # 主端口 6004,shard 端口 6005–6012
```

`--cluster`(或 `KEVY_CLUSTER=1`,或 `[cluster] enabled = true`)做
三件事:

- **per-shard listener**。shard `i` 拿到 `port + 1 + i` 的确定额外端口
  (用 `[cluster] port_base` 改 base)。主端口对其他东西保持完整代理
  风格行为。
- **真实拓扑报告**。`CLUSTER SLOTS / SHARDS / NODES` 通告实际分区:
  CRC16 `{hashtag}` slot,每 shard 一个连续 range。`CLUSTER KEYSLOT` /
  `COUNTKEYSINSLOT` / `MYID` 都实现,跟上游 Redis 一致。
- **`-MOVED` 替代转发**。一个落到 cluster 端口上的错 shard key 回
  `-MOVED <slot> <host:port>`,而不是被代理。正确路由意味着 `-MOVED`
  永不触发。

把已有数据目录切进/切出 cluster 模式会在启动时一次性 re-home key;源
文件备份为 `*.premigration.<ts>`。

现成的 cluster 感知工具 —— `redis-cli -c`、`redis-benchmark --cluster`、
主流客户端库 —— 都直接对 cluster 端口工作,因为协议子集忠实。

## 客户端 —— `ClusterClient`

`kevy-client` 1.9.0 ship 一个类型化路由客户端,你不需要完整第三方集群库:

```toml
[dependencies]
kevy-client = "1.11"
```

```rust
use kevy_client::ClusterClient;

// 连任一 cluster 端口作 seed;拓扑通过 CLUSTER SLOTS 发现,
// 每 shard 开一个连接。
let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;

cc.set(b"user:42", b"alice")?;
let v = cc.get(b"user:42")?;            // 路由到 user:42 owner shard
let n = cc.incr(b"counter")?;

// 多 key DEL/EXISTS 可跨 shard —— 每 key 路由,求和。
let removed = cc.del(&[b"a", b"b", b"c"])?;
# Ok::<(), std::io::Error>(())
```

runnable 版本在
[`crates/kevy-client/examples/cluster.rs`](../../crates/kevy-client/examples/cluster.rs):

```sh
kevy --port 6004 --threads 4 --cluster          # shard 在 6005-6008
cargo run -p kevy-client --example cluster -- 6005
```

### 路由怎么工作

1. **发现**。`connect` 发 `CLUSTER SLOTS` 给 seed,返回每 shard 的
   `[start, end, host, port]`。客户端建一个 `slot → shard-index` 表
   (16384 项),给每个不同 shard 节点开一个 `RespClient`。因为表来自
   服务器*实际*通告的 range,客户端不需要复制服务器的 `slot → shard`
   算法。
2. **路由**。每条单 key 命令计算 `key_hash_slot(key)`(如果有
   `{hashtag}` 就 CRC16 XMODEM,否则整 key)然后发到 slot owner 连接。
   无 `-MOVED`、无转发。
3. **需要时扇出**。`DBSIZE` / `FLUSHALL` 是全 cluster —— kevy 服务器
   端扇出(`Route::Dbsize` / `Route::Flush`),所以一次调用就报告/擦
   整个 cluster;客户端不自己 sum。

### 命令覆盖

| 类 | 命令 |
|-----|------|
| String | `set`、`set_with_ttl`、`get`、`incr`、`incr_by`、`expire`、`persist`、`ttl_ms` |
| Keys(多,按 key 路由) | `del`、`exists` |
| 全 cluster(服务器扇出) | `dbsize`、`flushall` |
| 无 key | `ping`、`publish` |
| Hash | `hset`、`hget`、`hdel`、`hlen`、`hgetall`、`hkeys`、`hvals` |
| List | `lpush`、`rpush`、`lpop`、`rpop`、`llen`、`lrange` |
| Set | `sadd`、`srem`、`smembers`、`scard`、`sismember`、`sinter`、`sunion`、`sdiff` |
| Sorted set | `zadd`、`zrem`、`zscore`、`zcard`、`zrange` |

没包装的东西,落到 raw 路由助手:

```rust
// 把任意单 key 命令路由到 owner shard。
let reply = cc.request_keyed(b"mykey", &[b"STRLEN".to_vec(), b"mykey".to_vec()])?;
// 任 shard 应答的无 key 命令。
let reply = cc.request_unkeyed(&[b"PING".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

### 多 key same-slot 限制

集合合并操作(`sinter` / `sunion` / `sdiff`)按它们**第一个** key 路
由。跟 Redis Cluster 一样,所有 key 必须在同一 slot —— 用共享
`{hashtag}` 让它们一起 hash:

```rust
cc.sadd(b"{users}:active",  &[b"a", b"b"])?;
cc.sadd(b"{users}:premium", &[b"b", b"c"])?;
let both = cc.sinter(&[b"{users}:active", b"{users}:premium"])?; // same slot → OK
# Ok::<(), std::io::Error>(())
```

没共享 hashtag 时 key 落到不同 shard,服务器答 `-MOVED`(以
`io::Error` 形式露出)。`del` / `exists` **不**这么受限 —— 它们独立
路由每个 key、把结果加起来。

Pub/sub **不**需要 cluster 感知的 subscriber:kevy 的 pub/sub 是进
程级(任一 shard publish 的消息会递给每个核的 subscriber),所以一个
普通 `Subscriber` 连任一端口都看见所有消息。`ClusterClient::publish`
同样只发到一个 shard。

## 性能

在干净的 lx64 16 核裸金属盒上测,服务器和客户端在不相交核,GET 工作
负载并发 64:

| 客户端路径 | 吞吐 | p99 延迟 |
|----------|----:|-------:|
| 单 shard 代理(跨 shard 跳) | 333 k ops/s | 3858 µs |
| **`ClusterClient`(零跳)** | **533 k ops/s** | **260 µs** |

那是 **吞吐 1.6×、尾延迟约 15× 下降** —— 纯粹来自去掉转发跳。类型化
`ClusterClient` 和手写裸 socket 路由打到同一上限,所以类型化 API 没
可测开销。用 `cargo run -p kevy-client --release --example cluster_bench`
复现。

> 在干净、核隔离的机器上跑 perf bench。在小型同居 cloud VM 上,跨
> shard 跳的代价淹没在调度噪音里 —— 这差点误导调查认为跳转无关紧要。

## 何时用

- **用 `ClusterClient`** —— 当单客户端推的负载大到转发跳显眼 —— 高吞
  吐或尾延迟敏感工作负载。这是自托管 kevy 在负载下的推荐路径。
- **维持普通 `Connection` / 单端口** —— 平常使用:代理行为正确且更简
  单,低负载下跳便宜。
- **抓 `redis-cli -c` / 第三方 cluster 客户端** 仅用于 interop 测试;
  原生 `ClusterClient` 对 Rust caller 更轻。

## 读写分离:集群模式 + 复制结合(v1.18)

`kevy-cluster-rw` 是给**复制**拓扑的姐妹客户端 —— 一个 primary kevy
节点服务写 + 一群 replica kevy 节点服务读(服务器端见
[`docs/replication.md`](replication.md))。它跟**集群模式正交**:复
制拓扑是每*进程*一个 writer,而集群模式把一个进程分成 N shard。它们
能组合 —— 集群模式下的 primary 通告 N shard,每个 replica 也跑 N shard,
运维在它们之间接 `kevy-cluster-rw`。

```rust
use kevy_cluster_rw::ReadWriteClient;

let mut client = ReadWriteClient::connect(
    ("primary.local", 6004),
    &[("replica1.local", 6004), ("replica2.local", 6004)],
)?;
// 写 → primary,读 round-robin 跨 replica(fleet 为空时回退 primary)。
// `consistent = true` 强制读走 primary。
client.request(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?;
let reply = client.request(&[b"GET".to_vec(), b"k".to_vec()])?;
# Ok::<(), std::io::Error>(())
```

crate 是 v1.18 release 添加。per-command 读/写分类在
`kevy_cluster_rw::is_write_verb`。v1.18 显式接 seed 列表(无自动
`CLUSTER NODES` 发现 —— release 后 follow-up);运维的部署脚本列
primary + replica 地址。

## embed-as-read-replica(v1.20 / Phase 2)

一个 `kevy-embedded` store 可订阅服务器 primary 的复制流并在进程内镜
像 keyspace。读零网络 round-trip;写本地被拒,必须走 primary。

```rust
use kevy_embedded::Store;

// 一行:in-memory replica,AOF 关,默认 reconnect(100 ms → 5 s)。
let replica = Store::open_replica("primary.local:16004")?;

// 读工作;写返 io::Error("READONLY ...")。
let v = replica.get(b"hello")?;
assert!(replica.set(b"k", b"v").is_err());
# Ok::<(), std::io::Error>(())
```

完全控制:

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

握手向 primary 的复制 listener(服务器端默认 `port + 10000`,通过服
务器的 `--replication-listener` / `[replication] listen_port` 配置)
发 `REPLICATE FROM <last-applied-offset> ID <replica_id>`。primary
ack offset 开始串流帧;embed runner 线程通过跟服务器端 replica 用同
样的 dispatch 路径把每帧应用到本地 shard,然后 advance 本地 offset
以便重连后续上。

### v1.20 范围(MVP)

- **单 upstream URL = 单 primary shard 镜像**。多 shard upstream 暂时
  是 "每 primary shard 起一个 `Store::open_replica`";per-URL runner
  便利接口在 follow-up 落地。
- **replica 上无本地 AOF**。`open_replica` 强制禁用(本地 AOF 跨重
  启发散且下次 open 时双重 apply)。跨 replica 重启的持久性:让
  upstream 的 backlog 保留够长让 replica 的 last-applied offset 仍在
  磁盘上。
- **无 snapshot ingest**。一个 replica 在 offset 0 连上一个 backlog 已
  滚过那点的 primary 时目前丢连接;完整 snapshot ingest
  (`+SNAPSHOT ... +SNAPSHOT_END`)是 v1.20.x follow-up。
- **`kevy-elect` ANNOUNCE 上无自动重定位**。primary 变更前手动 reconfigure,
  failover hook 之后落地;跟 `kevy-cluster-rw` 的拓扑刷新组合走全自动路径。
- **replica 上本地 PUBLISH 允许**。Pub/sub 在 kevy 里是进程本地的(不
  复制),所以本地 PUBLISH 只到本进程的订阅者;keyspace 本身保持只读。

### 故障模式

- **Primary down** —— runner 用指数退避 reconnect
  (`Config::with_replica_reconnect`,默认 100 ms → 5 s)。对最后应
  用的 snapshot 读继续工作;写仍返 `READONLY`。
- **Offset gap** —— wire 客户端露出 `OffsetGap`;runner 丢连接,下次
  reconnect 从新 applied offset 续上(现在落后 primary)。v1.20.x
  snapshot ingest 自动闭合这个 gap;v1.20 要求运维手动从 snapshot 刷
  新。
- **Replica drop** —— runner 线程在最后 `Store` clone drop 时 join;
  primary 的 listener 看到干净 FIN 释放 per-replica 槽位。

## scope 多 writer(v1.21 / Phase 3)

`kevy-scope` 让运维声明 per-前缀所有权:某具体 writer 节点拥有匹配
`<prefix>` 的 key 的写;其他节点全回 `-MISDIRECTED writer is
<host:port>` 让客户端 follow。可选 fallback 在 `kevy-elect` 标记
writer DOWN 时接管。

```toml
[cluster]
node_id = "embed-billing-1"
elect_port_base = 16100
peers   = "embed-billing-1@10.0.0.1:6004,server-eu-1@10.0.0.2:6004,reader-1@10.0.0.3:6004"
# prefix=writer[|fallback],逗号分隔。前缀里嵌入 `:` 没事(第一个 `=`
# 切前缀和 owner 规格)。
scopes  = "app:billing:=embed-billing-1|server-eu-1, app:auth:=embed-auth-1"
```

### 反范围(v3-cluster RFC 锁定)

- **无 Raft、无 gossip**。所有权表是静态 config;elect 仲裁只信号
  "writer DOWN → fallback 接管",不是拓扑共识。
- **迁移期无 write-shadow**。`MOVE-SCOPE` 跑 Q3=(a) quiesce-window:
  writer 暂停该前缀的写,船运它的 slice,然后所有权翻转。运维协调,
  无双接受窗口。
- **无自动迁移**。`MOVE-SCOPE` 运维触发;cluster 永不自决 scope 迁移。

### Wire 形状

- `-MISDIRECTED writer is <host:port>` —— 写落到不是该 scope writer
  (或活动 fallback)的节点。`kevy-cluster-rw` 1.21+ 缓存 per-key
  target 透明 retry;v1.20 及更早客户端传播错误。
- `-QUIESCED migrating to <host:port>` —— MOVE-SCOPE 窗内瞬时。客户
  端应短暂退避 retry 而不是 panic;quiesce 窗按 slice ship 时间界定
  (GB 级 scope 在 LAN 上单位数秒)。

### Embed 作 writer

一个 scope 的 writer 可以是 embed(`embed-as-writer`)或 server。
embed 的:

```rust
use kevy_embedded::{Config, Store};

let writer = Store::open(
    Config::default().with_embed_writer("0.0.0.0:6105")
)?;
// 每条本地写推进 embed 的复制源 backlog;reader 通过
// `kevy_replicate::ReplicaClient` 连 `0.0.0.0:6105`。
writer.set(b"app:billing:invoice:42", b"...")?;
# Ok::<(), std::io::Error>(())
```

### F4 fallback

当 `kevy-elect` 报 scope 的 writer 在 `down_peers` 里(最后 HB 比
`down_after` 默认 5 s 还老),声明的 fallback 把自己当活动 owner。
fallback 上的写成功;其他每个节点上的写继续 MISDIRECT,现在指 fallback。
当 writer HB 恢复,自动 reclaim 隐式 —— writer 离开 `down_peers`,所
以 fallback 退位。

**手动重入恢复(v1.21)**。如果 writer DOWN 够久让 fallback 接收过写,
那些写只在 fallback 上。在重新启用 writer 之前:停 writer,把 fallback
的数据目录复制到 writer 的,然后重启 writer。v3.1 通过从 fallback 流
的 writer-replica 握手自动化;v1.21 保持手动以维持 "无花哨共识" 契约。

### MOVE-SCOPE

```
MOVE-SCOPE <prefix> from <from-node-id> to <to-node-id>
```

对源 writer 发。走 Q3=(a) quiesce 窗:

1. writer 把它本地迁移状态翻成 `<prefix>` 的 MIGRATING;之后对该前缀
   的写答 `-QUIESCED migrating to <to-host:port>`。
2. writer 序列化前缀的 keyspace slice 并通过 `MOVE-SCOPE-INGEST
   <prefix> <bulk>` ship 到目标的数据端口。
3. `+OK` 后,writer 本地提交迁移:对该前缀的未来写在源上返
   `-MISDIRECTED writer is <to-host:port>`(无 quiesce —— 移动完了)。
4. 其他 cluster 成员按它们的静态 `[cluster] scopes` config 继续路由,
   直到运维推新 config + 重启(v1.21 无 gossip)。

**限制(v1.21)**:
- 迁移状态是 per-node 本地。其他 cluster 成员需要 config 推 + 重启才
  能学到新 writer。
- 数据 ship 内存中序列化整个前缀 slice。前缀 ≫ GB 时,在维护窗口期
  调度 MOVE-SCOPE;embed-as-writer 的 MVP 不为那个规模设计。
- 中途取消 ship 回到源 writer;目标上没留部分 apply 状态。
