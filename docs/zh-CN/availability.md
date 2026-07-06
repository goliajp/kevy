# 可用性

一个 kevy 3.x 部署如何在节点故障中保持可写可读:拓扑、从"最快、异步"一路到"多数派围栏"的一致性阶梯、计划内与崩溃切主,以及客户端在每一步看到的精确错误契约。

本文建立在 [`docs/replication.md`](replication.md)(流机制)之上 —— 如果你从没拉起过一对主/副本,请先读那篇。如果你只跑一个 kevy 节点,本文适用于你的只有阶梯的第一级。

## 拓扑

### 单节点

不配置复制(`role = "standalone"`,默认)。每次写入落入本地 store 即确认;耐久性由 [`docs/persistence.md`](persistence.md)(AOF + RDB)负责。没有切主 —— 可用性等于进程的存活时间。

### 主节点 + 副本

```toml
# primary                          # each replica
[replication]                      [replication]
role = "primary"                   role = "replica"
                                   upstream = "primary.internal:16004"
```

主节点按 shard 流出每次施加的改动;副本施加并服务读(客户端写得到 `-READONLY`)。两种接线形态:

- **Fleet**(默认):上游是一台完整的 kevy 服务器 —— 每个本地 shard 连接 `(host, port_base + shard_index)`,每 shard 一条流。
- **`single_source = true`**:上游是单端口上的一条流(通过 `with_embed_writer` 的 embedded writer)—— 一个路由 runner 按 key hash 把帧扇入本地 shard。见 [`docs/replication.md`](replication.md) 的*以 embedded 作主节点*一节。

这种拓扑下的切主是**手工的**:在幸存者上 `REPLICAOF NO ONE`,其余节点切换上游。

### 三节点 elect 多数派

```toml
# every node adds the same [cluster] block
[cluster]
node_id         = "n1"                  # unique per node
elect_port_base = 6204
peers           = "n1@10.0.0.1:6204:6004,n2@10.0.0.2:6204:6004,n3@10.0.0.3:6204:6004"
```

成员关系是**静态的**(运维方声明的 `peers` 表);角色是**动态的**(选举在这张表内部移动主节点)。法定人数是 `N/2 + 1` —— N=2 无法在任何故障中幸存(任一节点宕机都会把幸存者锁成只读),所以需要切主的部署使用 N ≥ 3。

请使用扩展的 peer 语法 `id@host:elect_port:client_port`:选举流量走 elect 端口,而切换上游与 `-MISDIRECTED` 回复用客户端端口。旧式两字段形式假定客户端端口等于 elect 端口,这几乎从来不是你想要的。

**只有选举才给写权威。** 在 elect 多数派中,`[replication] role = "primary"` 只是一个初始*偏好*。每个配置为 primary 的多数派成员都以**只读**状态启动,并扣住写入直到赢得选举 —— 冷启动也一样,集群在接受第一次写入前要付一轮选举(几秒钟)。正是这个无条件钳制防止了经典的"重启后的空主节点抹掉整个集群"事故:一个崩溃且丢盘的节点,永远不可能只凭配置就带着写权限回来。

## 一致性阶梯

复制默认是异步的。下面每一级都为某一个 verb 或某一个配置键买到一份更强的保证 —— 只为某次读或写真正需要的东西付费。

| 级 | 机制 | 保证 | 代价 |
|---|---|---|---|
| 0 | (默认)| 主节点本地施加后即 ack;副本尾随 | 无 |
| 1 | `REPL.TOKEN` + `REPL.WAIT` | 在选定副本上读己之写 | 副本上一次阻塞调用 |
| 2 | `WAIT n timeout` | 写入已在主节点上**且**被 ≥ n 个副本确认 | 主节点上一次阻塞调用 |
| 3 | `replica_max_staleness_ms` | 副本绝不提供老于界限的读(`-STALE`)| 滞后尖峰期间读转移到主节点 |
| 4 | `min_replicas_to_write` | 没有 n 个活副本时主节点拒绝写(`-NOREPLICAS`)| 写可用性与副本健康耦合 |
| 5 | 多数派租约(elect 下自动)| 被分区的主节点围栏自己的写(`-NOREPLICAS`)| 分区期间写暂停一个租约窗口 |

### 读己之写:`REPL.TOKEN` / `REPL.WAIT`

```
REPL.TOKEN
REPL.WAIT gen offset [gen offset ...] [TIMEOUT milliseconds]
```

先写主节点,然后在主节点上 `REPL.TOKEN`:它按 shard 返回一对 `(generation, offset)` —— 实时 feed 的尾部,按构造必然覆盖你的写入。把整个 token 交给你即将读取的那个副本上的 `REPL.WAIT`:它阻塞到每个 shard 都**施加**到至少那个位置,回答 `+OK`,你在这条连接上的下一次读就能观察到你的写。默认 `TIMEOUT` 是 1000 ms;`0` 与更大的值都被封顶在 60 s。

`generation` 这一半是防脚枪:它标识一段未曾断裂的 offset 历史(与 CDC feed 用的是同一个 generation —— 见 [`docs/cdc.md`](../cdc.md))。切主、`FLUSHALL` 或崩溃重启都会递增它,因此对着旧主节点 offset 空间铸造的 token 永远不可能在新主节点上假性满足 —— generation 不匹配时 `REPL.WAIT` 立即回答 `-MISDIRECTED writer is <primary>`,客户端回退去读主节点。超时给出同样的回复:两种情况下,"去读写者"都是唯一的恢复路径。

在主节点上,`REPL.WAIT` 立即返回 `+OK`(你已经在和写者说话),所以通过路由客户端无条件发出这个调用是安全的。

### `WAIT` —— 副本确认,不是耐久性

```
WAIT numreplicas timeout
```

阻塞到**每个 shard** 的 `master_repl_offset`(栅栏武装时冻结)都被至少 `numreplicas` 个副本确认,或超时;返回各 shard 中已确认副本数的最小值。全 shard 栅栏是有意的 —— kevy 的一次写可能落在任何 shard 上,所以按 shard 计数是唯一永远不会错的答案。`timeout 0` 是 Redis 的"永远等",硬封顶 60 s。在副本上发出 `WAIT` 会回答 `-ERR WAIT cannot be used with replica instances`。

**`WAIT` 不是耐久性。** 副本 ACK 意味着帧到达了副本的施加流水线,不意味着任何地方发生过 fsync。`WAIT 1` 真正买到的是:写入存在于两个节点上,因此它能在任何单节点损失中幸存,*前提是随后的选举挑选最领先的候选人* —— 而它确实这么做(见下文崩溃切主)。要跨断电耐久,把复制与主节点上的 AOF 配合使用。

### 有界陈旧:`-STALE`

```toml
[replication]
replica_max_staleness_ms = 2500     # 0 = off (default)
```

副本最近一次收到主节点心跳的时间早于界限时,以 `-STALE replica is stale; read the primary or raise replica_max_staleness_ms` 拒绝读。心跳以 1 Hz 随复制流而行,所以低于约 2 s 的界限在健康链路上也会触发;gate 用的是 2500 ms。心跳一恢复副本就自愈 —— 无需运维动作。

### 写闸门:`min_replicas_to_write` 与多数派租约

```toml
[replication]
min_replicas_to_write = 1           # 0 = off (default)
```

一个仿 Redis 的启发式:健康副本(有活跃复制连接且已 ACK)少于 N 个时,主节点以 `-NOREPLICAS Not enough good replicas to write.` 拒绝写。它关闭了"主节点向虚空写入"的窗口,但**不是**脑裂保证 —— 分区的两侧可以各自看到自己的副本。

真正的围栏是**多数派租约**,在 elect 多数派中自动生效:一个主节点的选举心跳在租约窗口(= `down_after`,5 s)内够不到严格多数的 peer 时,就以 `-NOREPLICAS primary lost quorum; writes fenced` 围栏自己的写,分区愈合后解除。与 `WAIT` 或 token 结合,这把"少数派一侧静默吸收了写入"压缩到最多一个租约窗口,且窗口内的每次写都*响亮地*失败,而不是静默分叉。

## 切主

### 计划内:`FAILOVER` verb

```
FAILOVER host port [TIMEOUT ms]      # host:port = the target replica's CLIENT address
FAILOVER ABORT
```

在主节点上运行;立即回答 `+OK`,在后台线程执行交接(异步,与 Redis 的 `FAILOVER` 一样):

1. **静默** —— 每个新的客户端写都回答 `-QUIESCED migrating to <host:port>`;[`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) 客户端已经会带退避地重试这些回复,所以写者是暂停而不是失败。
2. **追平** —— 旧主节点轮询目标的 `INFO replication`,直到 `master_link_status:up` 且 `slave_lag_frames:0`。写已静默时,收敛的仪表就是精确的 —— 这就是零丢失的那一步。
3. **提升 + 跟随** —— 向目标发送 `REPLICAOF NO ONE`(它的 feed generation 递增,把过期 token 围栏在外),然后旧主节点把自己切换到目标的复制端口,成为只读副本。
4. 解除静默。仍然指向旧节点的散落写入现在得到 `-READONLY` 并重路由。

`FAILOVER ABORT` 在提升之前的任何时点清除静默;后台线程注意到后收手。如果目标在 `TIMEOUT`(默认 10 000 ms)内始终没有追平,静默回滚,节点恢复主节点职责 —— 失败的尝试只花掉一次写可用性的闪断,别无其他。

一个寻址约束:交接把上游切到 `客户端端口 + 10000`,因此目标必须以默认的 `listen_port_base` 运行(见下文端口约定)。

### 崩溃:多数派选举

在每个节点上配好 `[cluster]` 块后,死亡的主节点会在没有运维介入的情况下被检测和顶替:

1. Peer 们在 `down_after`(5 s)内收不到它的选举心跳,标它 DOWN。
2. 一个具备资格的副本发起候选:它必须在在线 peer 中持有**最高的复制 offset**(各 shard 已施加流位置之和),`node_id` 最小者打破平局。按 offset 择优正是让 `WAIT` 确认过的写入得以幸存的机制 —— 持有你已确认写入的副本,排位高于没有的。
3. 候选人需要在 `election_timeout`(3 s)内拿到多数派的 `ACCEPT`;纪元与选票在离开节点*之前*就持久化到 `<data_dir>/elect.meta`,因此崩溃重启永远不会重复投票。
4. 胜者广播 `ANNOUNCE`,停掉自己的 runner 集群(写打开),并递增自己的 feed generation。败者自动把复制上游切到胜者。

**MTTR ≈ `down_after` + 一轮选举** —— 用出厂时序约 5–8 s;gate 把端到端(含重新开写)封顶在 30 s。选举时序(`hb_interval` 200 ms、`down_after` 5 s、`election_timeout` 3 s)在本版本中是固定常量,不是配置键。

**旧主节点回来了。** 重启角色钳制让它以只读启动(上文"只有选举才给写权威")。选举告诉它当前主节点是谁;它切换上游并握手。它在分区之后、死亡之前吸收的任何写入构成一段*分叉后缀* —— 它的流位置领先于新主节点,而新主节点用唯一安全的方式回答:**丢弃分叉**,发一份完整快照,衔接实时帧。重新加入的节点收敛到多数派的历史;分叉的写入没了(按定义它们从未被 `WAIT` 确认 —— 分叉恰恰就是未复制出去的尾巴)。

故障节点**不会**被自动顶替,成员关系在运行时永不改变 —— 换硬件意味着在每个节点上更新 `peers` 并重启它们。动态成员、多主、跨数据中心都不在章程内。

## 写者与读者的错误契约

完整目录在 [`docs/error-replies.md`](../error-replies.md);这张表是可用性切片 —— 客户端在拓扑生命周期的每个时点看到什么、该做什么。`kevy-cluster-rw::ReadWriteClient` 已经实现了所有这些行为(写到主节点、读在副本间轮询、跟随 `MISDIRECTED`/`QUIESCED` 重定向、强制走主节点的一致读路径)。

| 回复 | 你在跟谁说话 | 何时 | 客户端动作 |
|---|---|---|---|
| `-READONLY You can't write against a read only replica.` | 一个副本(含被降级或被钳制的前主节点)| `replica_read_only = true`(默认)时的任何客户端写 | 把写发到主节点;路由客户端会重新解析它 |
| `-QUIESCED migrating to <host:port>` | 一个正在 `FAILOVER` 中的主节点 | 静默窗口(交接步骤 1–3)| 带退避重试;交接落定后跟随 `<host:port>` |
| `-MISDIRECTED writer is <host:port>` | 一个副本(`REPL.WAIT`),或一个非拥有者节点(作用域写)| 读己之写无法服务(超时 / generation 不匹配),或作用域路由的写 | 到 `<host:port>` —— 当前写者 —— 去读(或写)|
| `-NOREPLICAS Not enough good replicas to write.` | 设置了 `min_replicas_to_write` 的主节点 | 健康副本少于 N | 退避重试;呼叫副本的运维负责人 |
| `-NOREPLICAS primary lost quorum; writes fenced` | 分区少数派一侧的多数派主节点 | 多数派租约丢失 | 退避重试 —— 要么分区愈合,要么多数派选出新主,你的路由客户端会找到它 |
| `-STALE replica is stale; read the primary or raise replica_max_staleness_ms` | 设置了陈旧界限的副本 | 主节点心跳老于界限 | 在副本追上之前去读主节点 |

两条经验法则:上面每一个都是**按设计可重试的**(什么都没被施加),每一个都点名了路由客户端自愈所需的拓扑真相 —— 没有一个需要人在环里。

## 运维

### 端口约定

| 平面 | 端口 | 备注 |
|---|---|---|
| 客户端 RESP | `server.port`(如 6004)| 客户端与 `peers` 里 client-port 指向的地址 |
| 复制 | `listen_port_base + shard_i`;默认 base = 客户端端口 + 10000 | `nshards` 个连续端口;从 v3.15 起**副本也绑定这个监听**(提升对称性)|
| 选举 | `elect_port_base`;默认 = 客户端端口 + 200 | 每节点一个控制面监听 |

自动切换上游(选举)与 `FAILOVER` 都假定 `客户端端口 + 10000` 的复制约定 —— 任何启用切主的部署都把 `listen_port_base` 留在默认值。在一台主机上跑多个实例:客户端端口至少相隔 `nshards`,否则实例的复制端口段会冲突。

### 配置键

`[replication]`(见 [`crates/kevy-config/src/replication.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-config/src/replication.rs)):

| 键 | 默认 | 含义 |
|---|---|---|
| `role` | `"standalone"` | `"primary"` 向副本流出;`"replica"` 从 `upstream` 拉取;standalone = 子系统休眠 |
| `upstream` | 未设置 | 仅副本:主节点复制端口基址的 `host:port` |
| `listen_port_base` | `0`(= 客户端端口 + 10000)| shard `i` 在 base + `i` 上绑定复制端口 |
| `replication_buffer_size` | `256mb` | 每 shard 环形 backlog;窗口内的重连跳过快照 |
| `reconnect_window_ms` | `60000` | 断开副本的 slot 保留多久 |
| `replica_read_only` | `true` | 在副本上拒绝客户端写(`-READONLY`);逃生舱是 `CONFIG SET` |
| `replica_max_staleness_ms` | `0`(关)| 阶梯第 3 级:超过界限的读回 `-STALE` |
| `min_replicas_to_write` | `0`(关)| 阶梯第 4 级:健康副本不足 N 时回 `-NOREPLICAS` |
| `min_replicas_max_lag_ms` | `10000` | 为第 4 级预留的新鲜度窗口;当前的健康检查计入有活跃连接且已 ACK 的副本 |
| `single_source` | `false` | 单流上游(embedded writer),而不是 per-shard 端口群 |

`[cluster]` 选举键(见 [`crates/kevy-config/src/cluster.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-config/src/cluster.rs)):`node_id`(≤ 32 B ASCII,唯一)、`elect_port_base`、`peers`(`id@host:elect_port[:client_port],...`)。除非 `node_id` 与 `peers` 都设置了,选举器保持休眠。

### 可观测性

**副本**上的 `INFO replication`:

| 字段 | 真相来源 |
|---|---|
| `role:slave` + `master_host` / `master_port` | 实时上游(运行时 `REPLICAOF` 优先于配置)|
| `master_link_status` | 3 s 内有流心跳落地则为 `up`,否则 `down` |
| `master_last_io_seconds_ago` | 最后一次心跳的年龄 |
| `slave_read_only` | `-READONLY` 闸门 |
| `slave_repl_offset` | 已施加的流位置 |
| `slave_lag_frames` | 主节点宣布的尾部减去已施加 —— **0 表示已追平** |

**主节点**上的 `INFO replication`:

| 字段 | 真相来源 |
|---|---|
| `role:master`、`master_repl_offset` | 应答 shard 的流尾部 |
| `connected_slaves` | 活跃的副本连接数 |
| `slaveN:ip=…,port=…,state=…,offset=…,sent=…,lag=…` | 逐副本:`state` 在它 ACK 过后为 `online`(之前是 `syncing`),`offset` 是它的**已确认**位置,`lag` 以帧计 |

一个注意点:INFO 的 offset 是应答这条连接的那个 shard 的采样 —— 跨进程可比的是副本自己的 `slave_lag_frames` 仪表和数据本身,而不是"主节点 INFO offset == 副本 INFO offset"。

`ROLE` 用 Redis 的数组形状给出同样的真相(`master` + offset + 逐副本 `[ip, port, acked-offset]`,或 `slave` + 上游);配置了 elect 多数派时,实时选举角色优先于 `REPLICAOF` 状态与配置两者。实践中盯滞后:在副本上轮询 `slave_lag_frames`(持续非零超过你的陈旧预算就告警),在主节点上用 `WAIT 1 <小超时>` 作为廉价的端到端"至少一个副本跟得上吗"探针。

### Gate

本文承诺的一切都是可执行的:[`bench/availgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/availgate.sh) 对真实进程跑 12 道钳制 —— 阶段 1(施加期间 READONLY、offset/lag 真值、链路断/通检测、逐副本 ack 真值、min-replicas),阶段 2(3 节点崩溃切主含 MTTR 上界、重启角色钳制 + 丢弃分叉的重新加入),阶段 3(WAIT 真值、20/20 轮读己之写 + 未来 token 的 MISDIRECT、SIGSTOP 主节点下的 `-STALE`、多数派租约围栏与解除)。如果本文的断言与 gate 有一天冲突,以 gate 为准 —— 请提文档 bug。

## 参见

- [`docs/replication.md`](replication.md) —— 流机制、快照发送、以 embed 作副本、backlog 容量估算。
- [`docs/cdc.md`](../cdc.md) —— `REPL.TOKEN` 共享的 `(generation, offset)` 游标模型。
- [`docs/error-replies.md`](../error-replies.md) —— 完整错误目录。
- [`docs/persistence.md`](persistence.md) —— 故事里耐久性的那一半(`WAIT` 不是)。
- [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md) —— 选举线协议。
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) —— 实现整套错误契约的客户端。
