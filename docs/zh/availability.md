# 可用性

本文讲 kevy 部署如何在节点故障中保持可读可写：有哪些拓扑，从“最快、异步”一路到“多数派围栏”的一致性阶梯，计划内与崩溃两种切主，以及客户端在每一步看到的精确错误契约。

本文以 [`docs/replication.md`](replication.md)（流机制）为基础——如果你还没拉起过一对主节点/副本，请先读那篇。如果你只跑一个 kevy 节点，那么本文只有阶梯的第一级和你有关。

## 拓扑

### 单节点

不配置复制（`role = "standalone"`，默认）。每次写入落进本地 store 即告确认；耐久性交给 [`docs/persistence.md`](persistence.md)（AOF + RDB）。没有切主可言——可用性就等于进程的存活时间。

### 主节点 + 副本

```toml,fragment
# primary                          # each replica
[replication]                      [replication]
role = "primary"                   role = "replica"
                                   upstream = "primary.internal:16004"
```

主节点把每一笔已应用的改动按 shard 流出；副本应用这些帧并对外提供读（客户端写入会收到 `-READONLY`）。接线有两种形态：

- **Fleet**（默认）：上游是一台完整的 kevy 服务器——每个本地 shard 连接 `(host, port_base + shard_index)`，每个 shard 一条流。
- **`single_source = true`**：上游是单端口上的一条流（通过 `with_embed_writer` 的 embedded writer）——由一个路由 runner 按 key hash 把帧扇入本地各 shard。见 [`docs/replication.md`](replication.md) 的“以 embedded 作主节点”一节。

这种拓扑下的切主是**手工的**：在幸存者上执行 `REPLICAOF NO ONE`，再把其余节点的上游指过去。

### 三节点 elect 多数派

```toml
# every node adds the same [cluster] block
[cluster]
node_id         = "n1"                  # unique per node
elect_port_base = 6204
peers           = "n1@10.0.0.1:6204:6004,n2@10.0.0.2:6204:6004,n3@10.0.0.3:6204:6004"
```

成员关系是**静态的**（运维方声明的 `peers` 表）；角色是**动态的**（选举在这张表内部挪动主节点）。多数派规模是 `N/2 + 1`——两节点扛不住任何故障（任意一台宕机都会把幸存者锁成只读），所以需要切主的部署至少三节点。

peer 请写成扩展语法 `id@host:elect_port:client_port`：选举流量走 elect 端口，切换上游和 `-MISDIRECTED` 回复用客户端端口。旧式的两字段写法会假定客户端端口等于 elect 端口，这基本不会是你要的效果。

**写权限只来自选举。**在 elect 多数派里，`[replication] role = "primary"` 只是一个初始“偏好”。每个配置为 primary 的多数派成员都以**只读**状态启动，扣住写入直到赢得选举——冷启动也不例外，集群要先付一轮选举（几秒钟）才接受第一笔写入。正是这道无条件钳制，挡住了经典的“重启后的空主节点抹掉整个集群”事故：一个崩溃且丢了盘的节点，永远不可能只凭配置就带着写权限回来。

## 一致性阶梯

复制默认异步。下面每往上一级，都用一个 verb 或一个配置键换来一份更强的保证——每次读写只为自己真正需要的保证付费。

| 级 | 机制 | 保证 | 代价 |
|---|---|---|---|
| 0 | （默认） | 主节点本地应用后即 ack；副本尾随 | 无 |
| 1 | `REPL.TOKEN` + `REPL.WAIT` | 在选定副本上读己之写 | 副本上一次阻塞调用 |
| 2 | `WAIT n timeout` | 写入已在主节点上**且**得到 ≥ n 个副本确认 | 主节点上一次阻塞调用 |
| 3 | `replica_max_staleness_ms` | 副本绝不提供超过界限的陈旧读（`-STALE`） | 滞后尖峰期间读切到主节点 |
| 4 | `min_replicas_to_write` | 新鲜副本不足 n 个时主节点拒写（`-NOREPLICAS`） | 写可用性与副本健康绑定 |
| 5 | 多数派租约（elect 下自动） | 被分区的主节点围栏自己的写入（`-NOREPLICAS`） | 分区期间写暂停一个租约窗口 |

### 读己之写：`REPL.TOKEN` / `REPL.WAIT`

```
REPL.TOKEN
REPL.WAIT gen offset [gen offset ...] [TIMEOUT milliseconds]
```

先写主节点，再在主节点上执行 `REPL.TOKEN`：它按 shard 返回一对对 `(generation, offset)`，即实时 feed 的尾部位置，天然覆盖你刚才那笔写入。把整个 token 带到你准备读取的副本上执行 `REPL.WAIT`：它会阻塞到每个 shard 都**应用**到至少那个位置，然后回答 `+OK`，这条连接上的下一次读就能看到你的写入。`TIMEOUT` 默认 1000 ms；`0` 和更大的值都封顶在 60 s。

`generation` 这一半是防误用的保险：它标识一段从未断裂的 offset 历史（CDC feed 用的也是同一个 generation——见 [`docs/cdc.md`](cdc.md)）。切主、`FLUSHALL`、崩溃重启都会递增它，所以拿旧主节点 offset 空间铸出的 token，永远不会在新主节点上误判为已满足——generation 不匹配时，`REPL.WAIT` 立即回答 `-MISDIRECTED writer is <primary>`，客户端退回去读主节点。超时也给同样的回复：两种情况的恢复路径只有一条，就是“去读写者”。

在主节点上，`REPL.WAIT` 立即返回 `+OK`（你已经在跟写者对话），所以路由客户端无条件发这个调用是安全的。

### `WAIT`——副本确认，不是耐久性

```
WAIT numreplicas timeout
```

阻塞到**每个 shard** 的 `master_repl_offset`（以屏障建立那一刻的值为准）都得到至少 `numreplicas` 个副本确认，或者超时；返回各 shard 已确认副本数的最小值。全 shard 屏障是刻意设计——kevy 的一笔写可能落在任何 shard 上，逐 shard 计数是唯一永远不会错的答案。`timeout 0` 对应 Redis 的“永远等”，硬封顶 60 s。在副本上执行 `WAIT` 会得到 `-ERR WAIT cannot be used with replica instances`。

**`WAIT` 不是耐久性。**副本的 ACK 只说明帧已进入副本的应用流水线，不代表任何一端做过 fsync。`WAIT 1` 真正买到的是：这笔写入存在于两个节点上，因此扛得住任何单节点损失——前提是随后的选举挑中最领先的候选人，而选举确实如此（见下文崩溃切主）。要想断电也不丢数据，复制要和主节点上的 AOF 搭配使用。

### 有界陈旧：`-STALE`

```toml
[replication]
replica_max_staleness_ms = 2500     # 0 = off (default)
```

副本上一次收到主节点心跳的时间超过界限时，就以 `-STALE replica is stale; read the primary or raise replica_max_staleness_ms` 拒绝读。心跳搭复制流的便车，每秒一次，所以界限低于约 2 s 时在健康链路上也会误触发；gate 用的值是 2500 ms。心跳一恢复，副本立即自愈——不需要运维动手。

### 写闸门：`min_replicas_to_write` 与多数派租约

```toml
[replication]
min_replicas_to_write = 1           # 0 = off (default)
```

这是仿照 Redis 的启发式规则：健康副本不足 N 个时，主节点以 `-NOREPLICAS Not enough good replicas to write.` 拒绝写入。健康的定义是：有活跃复制连接、已 ACK，**且**最近一次 ACK 比 `min_replicas_max_lag_ms`（默认 10 000 ms）新——停止 ACK 的停摆副本即使 TCP 连接还挂着，也会从计数里老化出去。这堵住了“主节点对着虚空写”的窗口，但**不是**脑裂保证——分区两侧可以各自看到自己那边的副本。

真正的围栏是**多数派租约**，在 elect 多数派中自动生效：主节点的选举心跳在租约窗口（= `down_after`，5 s）内够不到严格多数的 peer 时，就以 `-NOREPLICAS primary lost quorum; writes fenced` 围栏自己的写入，分区愈合后自动解除。配合 `WAIT` 或 token，“少数派一侧悄悄吸收写入”的窗口被压缩到最多一个租约窗口，而且窗口内的每笔写都**响亮地**失败，不会静默分叉。

## 切主

### 计划内：`FAILOVER` verb

```
FAILOVER host port [TIMEOUT ms]      # host:port = the target replica's CLIENT address
FAILOVER ABORT
```

在主节点上执行；立即回答 `+OK`，交接在后台线程完成（异步，与 Redis 的 `FAILOVER` 一致）：

1. **静默**——每笔新的客户端写都回答 `-QUIESCED migrating to <host:port>`；[`kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw) 客户端本来就会对这类回复带退避重试，所以写方只是暂停，不会失败。
2. **追平**——旧主节点轮询目标的 `INFO replication`，直到 `master_link_status:up` 且 `slave_lag_frames:0`。写入已静默，收敛后的仪表读数就是精确值——零丢失靠的就是这一步。
3. **提升 + 跟随**——向目标发送 `REPLICAOF NO ONE`（它的 feed generation 随之递增，把过期 token 围栏在外），然后旧主节点把自己指向目标的复制端口，变成只读副本。
4. 解除静默。仍然发到旧节点的零星写入此后收到 `-READONLY`，自行改道。

`FAILOVER ABORT` 在提升发生之前的任何时刻都能清除静默；后台线程发现后就收手。如果目标在 `TIMEOUT`（默认 10000 ms）内一直追不平，静默回滚，节点恢复主节点职责——一次失败的尝试只损失一小段写可用性，别无代价。

还有一条寻址约束：交接会把上游切到“客户端端口 + 10000”，所以目标必须用默认的 `listen_port_base` 运行（见下文端口约定）。

### 崩溃：多数派选举

每个节点都配好 `[cluster]` 块之后，主节点死掉会被自动检测、自动顶替，无需运维介入：

1. 各 peer 在 `down_after`（5 s）内收不到它的选举心跳，标记它 DOWN。
2. 有资格的副本发起竞选：它必须在在线 peer 中持有**最高的复制 offset**（各 shard 已应用流位置之和），平局时 `node_id` 最小者胜出。按 offset 择优，正是 `WAIT` 确认过的写入得以幸存的机制——持有你已确认写入的那个副本，排位高于没有的。
3. 候选人要在 `election_timeout`（3 s）内拿到多数派的 `ACCEPT`；纪元与选票在发出节点**之前**就持久化到 `<data_dir>/elect.meta`，所以崩溃重启也不可能重复投票。
4. 胜者广播 `ANNOUNCE`，停掉自己的 runner 集群（写随之打开），并递增自己的 feed generation。败者自动把复制上游切到胜者。

**MTTR ≈ `down_after` + 一轮选举**——按出厂时序约 5–8 s；gate 把端到端（含重新开写）封顶在 30 s。选举时序（`hb_interval` 200 ms、`down_after` 5 s、`election_timeout` 3 s）在本版本是固定常量，不是配置键。

**旧主节点回来了怎么办？**重启角色钳制让它以只读启动（见上文“写权限只来自选举”）。选举告诉它当前主节点是谁；它切换上游并握手。它在分区之后、死亡之前吸收的写入构成一段“分叉后缀”——它的流位置领先于新主节点，新主节点用唯一安全的方式回应：**丢弃分叉**，发一份完整快照，再衔接实时帧。重新加入的节点收敛到多数派的历史；分叉里的写入消失了（按定义它们从未被 `WAIT` 确认过——分叉恰恰就是没复制出去的那截尾巴）。

故障节点**不会**自动补位，成员关系在运行时也永不变化——更换硬件意味着在每个节点上更新 `peers` 并重启。动态成员、多主、跨数据中心都不在章程之内。

## 写方与读方的错误契约

完整目录见 [`docs/error-replies.md`](error-replies.md)；这张表是其中的可用性切片——拓扑生命周期的每个时点客户端会看到什么、该怎么做。`kevy-cluster-rw::ReadWriteClient` 已经实现了全部这些行为（写发主节点、读在副本间轮询、跟随 `MISDIRECTED`/`QUIESCED` 重定向、一致读路径强制走主节点）。

| 回复 | 你在跟谁说话 | 何时 | 客户端动作 |
|---|---|---|---|
| `-READONLY You can't write against a read only replica.` | 副本（含被降级或被钳制的前主节点） | `replica_read_only = true`（默认）时的任何客户端写 | 把写发到主节点；路由客户端会重新解析主节点地址 |
| `-QUIESCED migrating to <host:port>` | 正处于 `FAILOVER` 中的主节点 | 静默窗口（交接第 1–3 步） | 带退避重试；交接落定后跟随 `<host:port>` |
| `-MISDIRECTED writer is <host:port>` | 副本（`REPL.WAIT`），或非拥有者节点（作用域写） | 读己之写无法满足（超时/generation 不匹配），或作用域路由的写 | 到 `<host:port>`——当前写者——去读（或写） |
| `-NOREPLICAS Not enough good replicas to write.` | 设置了 `min_replicas_to_write` 的主节点 | 健康副本不足 N | 退避重试；并通知负责副本的运维 |
| `-NOREPLICAS primary lost quorum; writes fenced` | 分区少数派一侧的多数派主节点 | 多数派租约丢失 | 退避重试——要么分区愈合，要么多数派选出新主节点，路由客户端会找到它 |
| `-STALE replica is stale; read the primary or raise replica_max_staleness_ms` | 设置了陈旧界限的副本 | 主节点心跳超过界限 | 副本追上之前先读主节点 |
| `-LOADING kevy is loading the dataset in memory` | full-resync 进行中的副本 | 快照传送正在整体替换副本的键空间 | 等待重试——窗口以传送时长为界；`PING` / `INFO` / `HELLO` 照常应答，健康检查不受影响 |

两条经验法则：上面每一条都是**按设计可重试的**（服务端什么都没有应用），每一条都点名了路由客户端自愈所需的拓扑真相——没有一条需要人工介入。

## 运维

### 端口约定

| 平面 | 端口 | 备注 |
|---|---|---|
| 客户端 RESP | `server.port`（如 6004） | 客户端以及 `peers` 中 client-port 指向的地址 |
| 复制 | `listen_port_base + shard_i`；默认 base = 客户端端口 + 10000 | `nshards` 个连续端口；自 v3.15 起**副本也绑定这个监听**（提升对称性） |
| 选举 | `elect_port_base`；默认 = 客户端端口 + 200 | 每节点一个控制面监听 |

自动切换上游（选举）和 `FAILOVER` 都假定“客户端端口 + 10000”的复制约定——凡是启用切主的部署，`listen_port_base` 一律留默认值。同一台主机跑多个实例时，客户端端口至少间隔 `nshards`，否则实例之间的复制端口段会冲突。

### 配置键

`[replication]`（见 [`crates/kevy-config/src/replication.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-config/src/replication.rs)）：

| 键 | 默认 | 含义 |
|---|---|---|
| `role` | `"standalone"` | `"primary"` 向副本流出；`"replica"` 从 `upstream` 拉取；standalone = 子系统休眠 |
| `upstream` | 未设置 | 仅副本用：主节点复制端口基址的 `host:port` |
| `listen_port_base` | `0`（= 客户端端口 + 10000） | shard `i` 在 base + `i` 上绑定复制端口 |
| `replication_buffer_size` | `256mb` | 每 shard 的环形 backlog；窗口内重连可跳过快照 |
| `reconnect_window_ms` | `60000` | 掉线副本的槽位保留多久 |
| `replica_read_only` | `true` | 副本上拒绝客户端写（`-READONLY`）；逃生口是 `CONFIG SET` |
| `replica_max_staleness_ms` | `0`（关） | 阶梯第 3 级：超过界限的读回 `-STALE` |
| `min_replicas_to_write` | `0`（关） | 阶梯第 4 级：健康副本不足 N 时回 `-NOREPLICAS` |
| `min_replicas_max_lag_ms` | `10000` | 第 4 级的新鲜度窗口：副本只有在最近一次 ACK 比这个界限新时才算健康——停摆副本不再满足 `min_replicas_to_write` |
| `single_source` | `false` | 单流上游（embedded writer），不走 per-shard 端口群 |

`[cluster]` 选举键（见 [`crates/kevy-config/src/cluster.rs`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-config/src/cluster.rs)）：`node_id`（≤ 32 B ASCII，全局唯一）、`elect_port_base`、`peers`（`id@host:elect_port[:client_port],...`）。`node_id` 和 `peers` 缺一个，选举器就保持休眠。

### 可观测性

**副本**上的 `INFO replication`：

| 字段 | 真相来源 |
|---|---|
| `role:slave` + `master_host` / `master_port` | 实时上游（运行时 `REPLICAOF` 优先于配置） |
| `master_link_status` | 3 s 内有流心跳落地则为 `up`，否则 `down` |
| `master_last_io_seconds_ago` | 最后一次心跳距今多久 |
| `slave_read_only` | `-READONLY` 闸门状态 |
| `slave_repl_offset` | 已应用的流位置 |
| `slave_lag_frames` | 主节点宣告的尾部减去已应用位置——**0 表示已追平** |

**主节点**上的 `INFO replication`：

| 字段 | 真相来源 |
|---|---|
| `role:master`、`master_repl_offset` | **全部 shard 求和**后的流尾部（与选举 offset 最优排序用同一约定） |
| `connected_slaves` | 有活跃连接的**不同副本进程**数（一个副本的各 shard 流折叠为一条记录） |
| `slaveN:ip=…,port=…,state=…,offset=…,sent=…,lag=…` | 逐副本进程，以对端地址标识：`state` 在**每条** shard 流都 ACK 过后为 `online`（任何一条没 ACK 都是 `syncing`），`offset` 是跨 shard 求和的已确认位置，`lag` 以帧计 |

跨进程真正可比的，是副本自己的 `slave_lag_frames` 仪表和数据本身——两个不同进程的 INFO 输出之间做 offset 算术，只在同一个 generation 内才有意义。

`ROLE` 以 Redis 的数组形状给出同样的真相（`master` + offset + 逐副本 `[ip, port, acked-offset]`，或 `slave` + 上游）；配置了 elect 多数派时，实时选举角色优先于 `REPLICAOF` 状态和配置两者。实践中主要盯滞后：在副本上轮询 `slave_lag_frames`（持续非零、超过你的陈旧度预算就告警）；在主节点上用 `WAIT 1 <小超时>` 当廉价的端到端探针，回答“至少有一个副本跟得上吗”。

### Gate

本文承诺的一切都可执行验证：[`bench/availgate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/availgate.sh) 对真实进程跑 13 道钳制——阶段 1（应用期间的 READONLY、offset/滞后真值、链路断/通检测、逐副本 ack 真值、min-replicas），阶段 2（3 节点崩溃切主含 MTTR 上界、重启角色钳制 + 丢弃分叉的重新加入），阶段 3（WAIT 真值、20/20 轮读己之写 + 未来 token 的 MISDIRECT、SIGSTOP 主节点下的 `-STALE`、多数派租约的围栏与解除），阶段 4（横跨一整个 full-resync 传送窗口的 `-LOADING` 契约，`PING` 豁免）。哪天本文的断言和 gate 冲突了，以 gate 为准——请给文档提 bug。

## 参见

- [`docs/replication.md`](replication.md)——流机制、快照发送、以 embed 作副本、backlog 容量估算。
- [`docs/cdc.md`](cdc.md)——`REPL.TOKEN` 共用的 `(generation, offset)` 游标模型。
- [`docs/error-replies.md`](error-replies.md)——完整错误目录。
- [`docs/persistence.md`](persistence.md)——故事里耐久性的那一半（`WAIT` 不是）。
- [`crates/kevy-elect/docs/protocol.md`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-elect/docs/protocol.md)——选举线协议。
- [`crates/kevy-cluster-rw`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-cluster-rw)——实现整套错误契约的客户端。
