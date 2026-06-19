# 主从复制 + 读写分离客户端(v1.18 / v3-cluster Phase 1)

kevy v1.18 ships v3-cluster **Phase 1 功能核心**:一个 kevy 节点可以
作为 primary 把每次 mutation 串流推给 N 个只读副本,或作为 replica
连接 primary 镜像 keyspace。新客户端 crate `kevy-cluster-rw` 把写发到
primary、读 round-robin 跨 replica。

**反范围提醒**(plan 已锁;**不要**在 v1.18 issue 里提):

- 多 master / sharded-multi-master —— 每个 scope 只允许一个 writer。
- 跨 DC active-active / CRDT。
- Raft / 强日志复制。
- 在线 resharding / gossip 发现 —— peer 列表由运维声明。
- AUTH / TLS —— kevy 永久不在范围内。
- 链式复制(replica-of-replica)—— dispatch-without-emit 闸门防御误配,
  但 wire 形状只支持单跳。

自动仲裁失败切换(`kevy-elect`)是 Phase 1.5,**不**在 v1.18 里。手
动通过 `REPLICAOF NO ONE` 提升是 v1.18 的 failover surface。

## 服务器端

### Primary

```toml
# kevy.toml
[replication]
role = "primary"
listen_port_base = 16004      # shard i 把复制绑在这个 + i
replication_buffer_size = 268435456   # 256 MiB 每 shard ring backlog
reconnect_window_ms = 60000   # 给一个 replica 的 offset 保留这么久的槽位
```

Shard `i` 在 `listen_port_base + i` 上绑一个专用复制 TCP listener
(per Issue Ledger I2 —— 镜像 per-shard cluster listener 模式)。每条
应用的写编码为 RESP 信封(`*2\r\n:<offset>\r\n<argv>`),推进 per-shard
有界 ring backlog;reactor 的 pump 在每次循环把这些帧串流给每个连接
的 replica。

协议是 RESP3-扩展的([`crates/kevy-replicate/docs/wire.md`])。offset
是 `i64` 编码;10 M 写/秒下,i64::MAX 上限约 30 000 年外。

### Replica

```toml
[replication]
role = "replica"
upstream = "primary.example:16004"    # primary 的 listen_port_base
```

当 kevy 以 `role = "replica"` 启动时,服务器给每个本地 shard 派出一个
**runner 线程**。Runner `i` 开一个阻塞 TCP 连接到
`(upstream_host, upstream_port_base + i)`,发握手
(`REPLICATE FROM <offset> ID <replica_id>`),读 `+ACK <offset>`,然后
在线路流上循环。每个 `ReplicaEvent`(live frame,或者
`SnapshotBegin` / `SnapshotChunk` / `SnapshotEnd` 之一)通过 MPSC 通道
转发到匹配 shard 的 reactor 线程;shard 每 tick 排空一次通道,在
`ReplicatedApplyGuard` scope 内通过常规 dispatch 路径应用。

guard 在 apply 期间抑制本地 `ReplicationSource::push_mutation` —— 如
果没有它,一个安装了下游 listener 的 replica 会重发每条 apply 帧,offset
双计。v1.18 禁止链式复制;闸门是防御性的。

Snapshot 派送:如果 replica 请求的 `from_offset` 已不在 primary
backlog 里(TooOld),primary 通过 `kevy_persist::write_snapshot_to`
in-line 序列化 shard 的 keyspace,加前缀 `+SNAPSHOT\r\n`,串流
`$<chunk>\r\n` bulks,以 `+SNAPSHOT_END <ack_offset>\r\n` 结束。replica
累积 chunks,调 `kevy_persist::load_snapshot_from` 到本地 `Store`,然
后在 `ack_offset` 无缝续 live frame。

## 命令

| 命令 | 效果 |
|------|------|
| `ROLE` | 无 upstream 在跑时回 `master <offset> []`,作为 replica 跑时回 `slave <host> <port> connect 0`。`REPLICAOF` 的 live 状态优先于静态配置。 |
| `INFO replication` | role / connected_slaves / master_repl_offset(master)或 master_host / master_port / master_link_status(replica)。 |
| `REPLICAOF host port`(别名 `SLAVEOF`) | 停止任何在跑的 runner fleet,parse + 解析新 upstream,派出新 runner。回 `+OK`。 |
| `REPLICAOF NO ONE` | 停止每个 runner;降级到 standalone(本地 store **不**被清空 —— 运维自己决定 promote 前是否 FLUSH)。 |
| `CLUSTER NODES` | 应答节点的 role 标记反映 live 复制状态(`myself,master` 或 `myself,slave`)。 |

## 客户端 —— `kevy-cluster-rw::ReadWriteClient`

```rust
use kevy_cluster_rw::ReadWriteClient;

let mut client = ReadWriteClient::connect(
    ("primary.local", 6004),
    &[("replica1.local", 6004), ("replica2.local", 6004)],
)?;

// 自动路由:SET 走 primary,GET round-robin replica。
client.request(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?;
let reply = client.request(&[b"GET".to_vec(), b"k".to_vec()])?;

// READCONSISTENT —— 强制读走 primary(刚写完接着读)。
let reply = client.request_read(
    &[b"GET".to_vec(), b"k".to_vec()],
    /* consistent = */ true,
)?;
```

v1.18 显式接受 seed 列表 —— 没有自动 CLUSTER NODES walk 来发现 replica。
(release 之后的 follow-up 可以加一个 auto-discover overload,给 cluster
模式部署里希望客户端自己找 replica 的运维用。)

写/读分类在 [`kevy_cluster_rw::is_write_verb`]。表对齐 server 端的
`kevy::cmd::is_write_verb`;重复是故意的(这个 crate 只下游
`kevy-resp-client` —— 永远不依赖 server crate)。

## 运维 recipe

### 加新 replica

1. 启新 kevy,带 `[replication] role = "replica"` 和
   `upstream = "primary:16004"`。
2. runner 用 `from_offset = 0` 连接。primary 的 backlog 早已驱逐
   offset 0 → TooOld → snapshot ship。
3. snapshot 加载完成后,runner 从 `ack_offset` 续上,在 live frame 上
   常驻。

### 重定位一个跑着的 replica

```
REPLICAOF new-primary.example 16004
```

停旧的 runner fleet(socket 被 `Shutdown::Both`,让任何 in-flight 读
解除阻塞),parse 新 upstream,派出新 runner。毫秒级回 `+OK`。replica
的本地 store **保留** —— 新 primary 的帧落在上面。如果运维想干净
replay,前后接 `FLUSHALL`。

### 手动提升(replica → primary)

```
REPLICAOF NO ONE
```

停止每个 runner。生效 role 翻成 `master`。本地 store 停在最后一帧应
用后的状态。要接受下游 replica,还需更新配置(`role = "primary"` +
`listen_port_base`)并重启 —— v1.18 **不**动态安装下游 listener。

## 通过 `kevy-elect` 的自动失败切换(v1.19+ / Phase 1.5)

v1.19 在 v1.18 的手动 `REPLICAOF` 上加了仲裁式 primary 失败切换。检测
通过心跳(`HB(epoch, node_id, role, repl_offset)`)每 `hb_interval_ms`
(默认 200 ms)一次;一个 peer 在 `down_after_ms`(默认 5 s)无心跳后
标记 DOWN;活着的 replica 中 `repl_offset` 最高(并列时 `node_id` 最
小)的广播 `OFFER(new_epoch, candidate_id, repl_offset)`;收集到
`N/2 + 1` `ACCEPT` 后通过现有 `REPLICAOF NO ONE` 路径自我提升,广播
`ANNOUNCE(epoch, new_primary_id, new_primary_addr)`。收到 `ANNOUNCE`
的 peer 把它们的 `kevy-replicate` runner 重定位到新 primary。完整规
范:[`crates/kevy-elect/docs/protocol.md`](../../crates/kevy-elect/docs/protocol.md)。

### 配置

```toml
[cluster]
node_id = "primary-east"              # 本节点稳定 id(≤ 32 B ASCII)
elect_port_base = 16104               # 控制面 TCP 端口(shard 0 = base + 0)
peers = "primary-east@10.0.0.1:16104,replica-1@10.0.0.2:16104,replica-2@10.0.0.3:16104"
```

`peers` 字符串列**集群里每个**节点(包括本节点)—— elector 运行时按
`node_id` 过滤自己。空 `peers` ⇒ kevy-elect 休眠(v1.18 时代的配置无
需改)。

### 仲裁 / 容错

| N | 仲裁 | 容忍 |
|---|----|----|
| 3 | 2 | 1 down |
| 5 | 3 | 2 down |
| 7 | 4 | 3 down |
| **2** | **2** | **0 down —— 退化,故意锁定** |

**N=2 警告**。仲裁是 `N/2 + 1`,所以 N=2 需要俩节点都活着:任何一个
down 都意味着幸存者达不到仲裁,**永久只读**(不接受写、不能 promote)。
这是故意的 —— 替代方案(单节点仲裁)会让分区时双写脑裂。配置 linter
在 `peers` 恰好列出两项时启动时 warning。**建议:N ≥ 3** 对任何需要
自动切换的部署。N=2 只在"任何一个 down = 锁定"比"俩 down = 锁定"
更优时可接受(非常罕见)。

### 脑裂保护

仲裁语义结构上防脑裂:分区少数边达不到 `N/2 + 1` ACCEPT,所以不能 promote 新
primary。一旦分区愈合,少数边看到多数边更高的 epoch,干净降级 ——
代价是丢掉分区期间落到少数边的写。这是 v3-cluster Phase 1.5 提供的持
久性故事:**写只在任何分区的多数边有持久性保证**。用 `READCONSISTENT`
避免陈旧读;写侧不能事后修复少数边的写。

### 可调项

| 参数 | 默认 | 作用 |
|------|-----|------|
| `hb_interval_ms` | 200 | 每 peer 出站 HB 的周期 |
| `down_after_ms` | 5_000 | 这么多毫秒无 HB 后标记 peer DOWN |
| `election_timeout_ms` | 3_000 | candidate 等仲裁 ACCEPT 这么久 |
| `election_backoff_ms` | 1_000–5_000 | 失败选举后退随机抖动 |

按你的 RTT 调 `hb_interval` × `down_after`。默认假定单 LAN。WAN 部署
(v1.19 反范围 —— kevy-elect 只单 DC)需要更高值避免临时 WAN 抖动触发
误选举。

### Backlog 调优

`replication_buffer_size` 是 per-shard ring 字节预算。粗略规则:

```
backlog_size ≈ 峰值写每秒 * 平均 argv 字节 * reconnect 窗口秒数
```

200k 写/秒、40 B 平均 argv、60 s 窗口,每 shard 480 MiB 让每次重连都
走 backlog 路径。更小的 backlog 没问题 —— 超大时干净回落到 snapshot
ship。

## 已知 v1.18 简化(作为 follow-up 跟踪)

- **后台 snapshot 序列化** —— *v1.18 已落地*。primary 冻结 COW
  `SnapshotView`(O(n) 浅 clone —— ns/entry)然后交给 worker 线程在
  reactor 外序列化;chunk 通过通道流回。reactor pause 缩到只收集。
- **per-replica peer-addr** —— *v1.18 已落地*。ROLE master 应答按每个
  连接的 replica 携带 `(ip, port, offset)`;`INFO replication` 的
  `connected_slaves` 来自这个列表。
- **io_uring 上的复制** —— *v1.18 已落地*。io_uring reactor tick 路径
  驱动 replica 的 accept / read / write / pump;吞吐敏感的写侧保持
  io_uring 原生(短写 + 现有非阻塞 drain)。`KEVY_IO_URING=1` + 复制
  跑动,匹配 epoll reactor 的 perfgate 数字。
- **CLUSTER NODES live-replica 列表** —— primary 当前不追踪其连接
  replica 的客户端侧地址(runner 的 REPLICATE 握手只带 id)。客户端
  用 `kevy-cluster-rw` 配显式 seed 替代。
- **Auth / link 加密** —— 永不(反范围)。

## Wire 格式参考

- Live 帧信封:[`crates/kevy-replicate/docs/wire.md`]。
- Snapshot ship:[`crates/kevy-replicate/docs/snapshot.md`]。
- 握手:`*5\r\n$9\r\nREPLICATE\r\n$4\r\nFROM\r\n$<n>\r\n<offset>\r\n$2\r\nID\r\n$<m>\r\n<replica_id>\r\n` → `+ACK <offset>\r\n`。

## 另见

- [`docs/cluster.md`](cluster.md) —— 多 shard 暴露 + slot 路由
  `ClusterClient`;跟复制正交(但可组合)。
- [`docs/persistence.md`](persistence.md) —— RDB / AOF;snapshot 路径
  复用 kevy-persist 的 wire ship 格式。
- `.claude/plans/2026-06-18-v3-cluster-plan.md` —— 规范执行计划;行状
  态反映本 release 里的内容。

[`crates/kevy-replicate/docs/wire.md`]: ../../crates/kevy-replicate/docs/wire.md
[`crates/kevy-replicate/docs/snapshot.md`]: ../../crates/kevy-replicate/docs/snapshot.md
[`kevy_cluster_rw::is_write_verb`]: ../../crates/kevy-cluster-rw/src/lib.rs
