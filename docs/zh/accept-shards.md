# `--accept-shards N`——把稀疏连接收拢到部分 shard 上

`--accept-shards N` 告诉 kevy：只在 shard `0..N` 上绑定 listener 并挂 `accept`，其余 shard 退化成纯计算 worker，但仍然服务跨 shard 的读写。

## 什么时候需要它

每个 kevy shard 各跑一个 reactor，主体是一个 busy-poll 循环。只有当每一轮迭代都能看到足够多的有效事件时，这个循环才摊得平它的逐轮开销（drain waker、查 inbox、挂 accept）。一旦 `concurrent_conns / threads` 掉到 1 以下，每个 shard 的大部分迭代都在空转，总吞吐会低于同一台机器上 1 到 2 个线程的服务器所能达到的水平。

以下场景该考虑 `--accept-shards N`：

- 客户端数量少而且已知（典型的应用服务器、复制 follower、内部服务）。
- 你仍然想让 `--threads` 跟核数对齐，好让跨 shard 一跳时键空间是并行的。
- 基准显示：加 shard 反而让吞吐变差。

连接数高（平均每 shard ≥ 1 条）、不可预测，或者来自会惊群的客户端连接池时，别设它——默认的“每个 shard 都 accept”才是那里的正确答案。

## 核心思路

accept 集合在启动时静态声明。集合内的 shard 绑定 listener fd 并挂一个 `accept` SQE；集合外的 shard 两件都不做，于是内核的 `SO_REUSEPORT` 组永远不会把 SYN 路由给它们。集合外的 shard 依然持有自己那片键空间，也依然会 drain 跨 shard inbox：哈希落到它那片的写入会被分派过去执行，回复再经由持有该连接的 shard 送回。无状态 shard 模型完好无损——每个 shard 跑的是同一条代码路径，只不过有些恰好持有零个连接。

```
                      clients (SO_REUSEPORT group)
                                 |
                  +--------------+--------------+
                  |              |              |
              shard 0        shard 1        shard 2          <-- accept-set (--accept-shards 3)
              listen + accept + own conns + own keyspace slice
                  |              |              |
                  +------ cross-shard dispatch -------+
                  |       |       |       |       |       |
              shard 3  shard 4  shard 5  shard 6  shard 7  shard 8  shard 9   <-- compute-only
              no listener, no accept; execute dispatched cmds, own keyspace slice
```

## 实例

10 个线程、3 个 accept shard，监听标准 Redis 端口：

```sh
kevy --threads 10 --accept-shards 3 --port 6379
```

等价的环境变量形态：

```sh
KEVY_THREADS=10 KEVY_ACCEPT_SHARDS=3 kevy --port 6379
```

等价的 TOML（`kevy.toml`）：

```toml
[server]
threads = 10
accept_shards = 3
port = 6379
```

优先级是 CLI > env > TOML > 默认值。`accept_shards` 必须满足 `1 <= accept_shards <= threads`；越界就在启动时快速失败，退出码 2。默认是不设置，也就是每个 shard 都 accept，此时二进制的行为与一个没有这个标志的构建逐字节一致。

## 定容经验法则

拇指规则：`accept_shards ≈ ceil(conns / 20)`。实测的甜点是一片平台区，每个 accept shard 大约持有 15–25 条连接——密到足以让 busy-poll 循环摊平开销，又稀到不会让跨 shard 分派把 inbox 通道打满。

| 并发连接数 | threads | 推荐 `--accept-shards` | 每 accept shard 的连接数 |
|-----------:|--------:|-----------------------:|-------------------------:|
| 10               | 10      | 1                             | 10                   |
| 20               | 10      | 1                             | 20                   |
| 50               | 10      | 2 或 3                        | 25 或 17             |
| 100              | 10      | 5                             | 20                   |
| 200              | 10      | 10（默认，不设）              | 20                   |
| 50               | 16      | 2 或 3                        | 25 或 17             |
| 100              | 16      | 5 或 6                        | 20 或 17             |
| 500              | 16      | 16（默认，不设）              | 31                   |

在标准的 `--threads 10 -c 50 -d 65536 SET` 负载上，`--accept-shards 3` 把 50 条连接从约 5 条/shard 收拢到约 17 条/shard，吞吐比默认高 +10.6%。`--accept-shards 2`（25 条/shard）落在同一片平台区上。

## 取舍

| 维度 | accept 集合内的 shard | 纯计算 shard |
|---|---|---|
| busy-poll 摊销 | 高（自己持有的连接扇入驱动每轮的活） | 取决于跨 shard inbox 的速率 |
| 逐连接的读 / 写成本 | 本地，无跳 | 不适用（不持有连接） |
| 跨 shard 分派成本 | 每跳一次 channel send | 一次 channel recv + 执行 + 回复 send |
| 键空间归属 | 持有自己那片 | 持有自己那片 |
| 空闲时的 CPU 下限 | 挂着 accept，阻塞在 `io_uring_enter` | 自旋到 `URING_SPIN_LIMIT` 后阻塞 |
| 对尾延迟的影响 | accept SQE 给热循环加活 | 热循环更干净；跨 shard 一跳多一个 channel |

具体地说：`N` 选得越小，每条连接从“热的 accept shard”里得到的收益越大，但要走跨 shard 一跳的写入占比也越高（全部键里的 `(threads - N) / threads`）。每 shard 15–25 连接这片平台区，正是 busy-poll 的收益付得起额外跳数的地方；明显低于它，分派成本占了上风；明显高于它，你又退回到不设置的默认状态了。

## FAQ

**是不是总该设 `--accept-shards`？**
不是。如果平均下来 `concurrent_conns / threads >= 1`，默认（每个 shard 都 accept）已经是最优——busy-poll 循环不用人帮就能摊平。`--accept-shards` 是给稀疏连接这个区间用的，那里加 shard 反而伤吞吐。

**怎么给我的负载挑 `N`？**
从拇指规则 `ceil(conns / 20)` 起步。在 perfgate 里把 `N` 在 `{ceil(conns/25), ceil(conns/20), ceil(conns/15)}` 之间扫一遍，在你的目标尾延迟预算下挑吞吐最高的那个。落在 15–25 连接/shard 这片平台区上的任何取值，都是安全的生产设置。

**它会不会破坏复制、AOF 或持久化？**
不会。集合外的 shard 跑的是同一条 reactor 循环、同一个 TTL reaper、同一个 AOF writer、同样的复制 tick。它们唯一跳过的，就是绑定 listener 和挂 `accept`。复制 follower 以及快照 / AOF 恢复的行为，与 `N` 取什么值无关。

**集合外的 shard 怎么还能服务写入？**
它们持有自己那片键空间。当 accept 集合内某条客户端连接发来一笔写入，而键哈希落到某个纯计算 shard 上时，accept shard 会把一个 `Inbound::RequestBatch` 推进那个持有者 shard 的 inbox；持有者 shard 对自己那片键空间执行命令，再把回复经由发起 shard 的回复通道送回。从客户端看，这一跳是隐形的——它在一条连接上看到一条回复，顺序不变。

**万一数字选错了？**
`N` 选得太小，代价是跨 shard inbox 被打满、尾延迟变差；`N` 选得太大，代价是回到最初那个稀疏连接的反转。两者都可以换个值重启就恢复——这个选择不绑定任何磁盘上的状态。
