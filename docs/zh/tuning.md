# 调优 kevy

本页汇总影响 kevy 单次操作开销的运行时旋钮——CPU 布局、reactor 选择、持久化、内存上限、网络传输，以及几个 Linux 侧的手段。

## 何时需要

遇到下面这些情况再来翻这一页：

- bench 结果显示 kevy 达不到你的吞吐或延迟目标，你想知道下一个该拧哪个旋钮。
- 部署 kevy 的主机上，默认配置（TCP loopback、io_uring 自动检测、`appendfsync everysec`、不设 `maxmemory`）和负载对不上——比如稀疏连接的服务、要靠 NVMe 保证耐久性的场景，或是设了内存上限的缓存层。
- 你在用 `perf` 给 kevy 做 profiling，需要保留调试行表的构建 profile。

如果只是在笔记本上把 kevy 跑起来，数字看着也正常，那不需要读这一页。默认值已经调到对各类负载都比较合理。

## 核心思路

kevy 是 thread-per-core 服务器：每个 OS 线程跑一个 shard，键空间按 CRC16 hashtag 划分、shard 间 shared-nothing，每个 shard 上跑一个忙轮询 reactor。默认值追求“什么负载都不差”；调优的意思是让 shard 数、reactor 和持久化策略对准**性能数据实际指出的瓶颈**。不要预防性地拧旋钮：先测量，找到开销所在，再一次只改一个变量。

## 调优 playbook

### CPU 与 shard

| 旋钮 | 位置 | 默认 | 效果 |
|------|------|------|------|
| `--threads N` / `KEVY_THREADS` | CLI / env | 在线核数 | shard 数；每个 shard 一个 OS 线程 |
| `--accept-shards K` | CLI | 所有 shard 都 accept | 只有前 K 个 shard 绑定监听，其余 shard 只做计算 |
| CPU 绑核 | `taskset` / `numactl` | 无 | 把 shard 锁在固定的核集合上 |

**如何选 `--threads`。** 把它设成负载里真实存在的并行度。单客户端 pipeline bench（`-c 1 -P 16`）只能打满一个 shard；这时设 `--threads 10`，剩下九个 shard 只会空转忙轮询，还会跟 shard 0 抢 cache line。真实的多客户端负载，从 `min(cores, expected concurrent clients / 4)` 起步，然后实测。

**如何选 `--accept-shards`。** 连接数和 shard 数之比很低时（稀疏连接负载——比如 50 个客户端摊在 10 个 shard 上，每 shard 只有 5 个连接），忙轮询每轮迭代的固定开销摊不下去，吞吐随之下滑。经验法则是 `ceil(conns / 20)`：50 个连接就设 `--accept-shards 3`，让三个监听 shard 各自接下大约 17 个连接，其余 shard 只做计算，但仍会通过内部 dispatcher 收到跨 shard 的工作。实测的甜点区间比这个单点估计更宽；完整的参数扫描，以及跨 shard 转发的开销何时会盖过 accept 集中的收益，见 [docs/accept-shards.md](https://github.com/goliajp/kevy/blob/develop/docs/accept-shards.md)。

**CPU 绑核。** 在 bench 机或单租户主机上，把 kevy 绑在固定的核集合上，可以让 NIC IRQ → softirq → 用户线程这条路径始终落在同一组 L1/L2 上：

```sh
taskset -c 0-9 kevy --port 6004 --threads 10
```

客户端如果跑在同一台机器上，要把服务端和客户端绑到**互不重叠**的核区间（服务端 `0-9`，客户端 `10-15`）。共用核心会重新引入调度器乒乓，足以吞掉 reactor 层的全部收益。

### Reactor 选择

| 平台 | 默认 | 覆盖 |
|------|------|------|
| Linux ≥ 5.19 | io_uring（自动检测） | `KEVY_IO_URING=0` 强制 epoll；`KEVY_IO_URING=1` 强制要求 io_uring，若 `io_uring_setup` 被 seccomp 拦截则高调报错退出 |
| macOS / *BSD | kqueue | 不可配置 |
| 较老的 Linux | epoll | 不适用 |

Linux 上的自动检测会在启动时执行一次 `io_uring_setup`；如果这个 syscall 被拦下（seccomp profile、锁死的容器），kevy 会静默回退到 epoll。在加固部署里，如果你宁可让它高调失败也不接受静默降级，就设 `KEVY_IO_URING=1`，io_uring 不可用时服务器直接拒绝启动。反过来，想做可复现的 epoll 与 io_uring 对比 bench，或者要绕开某个内核回归，需要把 io_uring 排除在外时，设 `KEVY_IO_URING=0`。

```sh
KEVY_IO_URING=1 kevy --port 6004   # require io_uring, exit if blocked
KEVY_IO_URING=0 kevy --port 6004   # force epoll
```

### 持久化

AOF 策略由 `appendfsync` 控制（配置文件或 `CONFIG SET`）。三个取值与 Redis 语义一致：

| `appendfsync` | 耐久性 | 代价 |
|---------------|--------|------|
| `always` | 每次写入都先 `fsync` 再回复 | 延迟最高；下限是 NVMe 的 sync 延迟 |
| `everysec`（默认） | 后台线程每秒 `fsync` 一次 | 数据丢失窗口不超过 1 s；热路径开销近乎为零 |
| `no` | 从不 `fsync`，内核按自己的节奏落盘 | 最快；数据丢失窗口 = page-cache 的刷盘间隔 |

`everysec` 的后台 `fsync` 跑在独立的 bio 线程上，不在 shard 热路径里，所以 shard 的尾延迟不会跟磁盘延迟耦合。纯缓存或只读副本还可以用 `--no-aof` 彻底关掉 AOF（完全不写 AOF 文件，连缓冲都没有）。

### 内存

| 旋钮 | 默认 | 作用 |
|------|------|------|
| `maxmemory` | 不限 | 内存硬上限（字节）；达到后驱逐策略开始生效 |
| `maxmemory-policy` | `noeviction` | 触顶时丢弃哪些键 |
| `maxmemory-samples` | 5 | 近似 LRU/LFU 策略的采样数 |

驱逐策略与 Redis 相同：`noeviction`、`allkeys-lru`、`allkeys-lfu`、`allkeys-random`、`volatile-lru`、`volatile-lfu`、`volatile-random`、`volatile-ttl`。`noeviction` 在触顶后让写入直接报 OOM，是主存储的安全默认值；`allkeys-*` 系列适合任何键都可以丢的缓存层。

`maxmemory-samples` 是近似策略的质量与成本旋钮——采样越多，越接近真正的 LRU/LFU，代价是每次驱逐多花一点 CPU。默认值 5 对大多数缓存负载已经够用；如果你的访问模式下驱逐明显选错了对象，可以提到 10；只有当驱逐本身出现在 profile 里时，才考虑降到 3。

**容器要按 `process_rss_bytes` 定容，不是 `used_memory`。**`INFO memory` 两个都报：`used_memory` 是 store 的键空间记账——`maxmemory` 和分层预算作用的对象；`process_rss_bytes` 才是操作系统真正为进程保留的常驻内存，它额外承载着索引与视图、连接与复制缓冲区、分配器开销与碎片。按 `used_memory` 设容器内存上限会把一个健康的进程 OOM 杀掉；请按观察到的 RSS 加余量来设限。

**可选启用的分配器。** 用 `--features kevy-alloc` 构建，会把 glibc malloc 换成 kevy 自己的 span 分配器：churn 之下的稳态 RSS 小约 10 %，而吞吐上的代价只出现在写饱和的集合分片上。当内存容量是那条约束时，这个构建是值得的；实测的取舍见 [docs/alloc.md](https://github.com/goliajp/kevy/blob/develop/docs/alloc.md)。

### 网络

默认传输是 TCP。客户端和服务端在同一台主机上时，可以换成 Unix-domain socket，完全绕开 loopback 的 TCP 栈：

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
redis-cli -s /tmp/kevy.sock SET foo bar
```

服务器是双绑定的：TCP 继续服务远程客户端，UDS 负责本地客户端，RESP 语义和 shard 运行时都完全一样。本地客户端负载上的收益很大（小载荷下 loopback TCP 路径是最大的开销来源）；完整数据、权限模型，以及 UDS 不适用的场景，见 [docs/uds.md](uds.md)。

**绑定地址警告。** kevy 目前既没有 AUTH 也没有 TLS。绑定到非 loopback 地址（`--bind 0.0.0.0` 或任何公网接口）会在启动时打印警告，因为此时网络上的任何一方都能直接下发命令。请把 kevy 放在私有网络边界之内，或者放在负责认证的代理后面。

**连接内省。**`INFO clients` 报告跨全部 shard 求和的实时 `connected_clients` 仪表。`CLIENT LIST` / `CLIENT INFO` 为每条真实客户端连接渲染一行 Redis 7.x 形状的记录——对端地址、全局唯一 `id`、`name`、订阅计数、MULTI 队列深度、输入/输出缓冲大小（`cmd=NULL`：不跟踪最近命令名）。`CLIENT SETNAME` 给连接打标签供 LIST 查看；`CLIENT KILL ID <id> | ADDR <ip:port> | LADDR <ip:port>`（或旧式位置参数 `CLIENT KILL <ip:port>`）关闭所有匹配的连接，包括停在阻塞命令里的连接。拆连接前会等受害者的待发输出排空，所以杀掉自己的连接依然能收到自己那条回复。

### 复制与可用性

只在跑主节点/副本拓扑时才需要关心（[docs/replication.md](replication.md)、[docs/availability.md](availability.md)）。

**端口布局。** 每个节点用三个平面，默认都从客户端端口派生：

| 平面 | 端口 | 备注 |
|------|------|------|
| 客户端 RESP | `port`（如 6004） | 客户端连的端口，`peers` 里的 client-port 指的也是它 |
| 复制 | `listen_port_base + shard_i`；默认 base = `port` + 10000 | 连续 `nshards` 个端口；副本也绑定同一段（v3.15） |
| 选举 | `elect_port_base`；默认 = `port` + 200 | 每个节点一个控制面监听 |

一台机器上部署多个实例时，客户端端口之间至少要隔开 `nshards`，否则默认的复制端口段会互相冲突。`FAILOVER` 和自动重挂上游都建立在 `port + 10000` 这一约定上——启用切主的部署，请把 `listen_port_base` 留在默认值。

**一致性旋钮。** `[replication]` 下有两个键，用可用性换更强的保证（完整的层级见 [docs/availability.md](availability.md)）：

| 旋钮 | 默认 | 作用 |
|------|------|------|
| `replica_max_staleness_ms` | `0`（关闭） | 副本收到的最后一次主节点心跳早于该阈值时，用 `-STALE` 拒绝读；心跳以 1 Hz 随复制流下发，所以阈值低于约 2 s 时健康链路也会误触发 |
| `min_replicas_to_write` | `0`（关闭） | 健康副本少于 N 个时，主节点用 `-NOREPLICAS` 拒绝写 |

如果只想在单次调用上设栅栏，而不是长期改配置，代价就是阻塞那一次调用：主节点上用 `WAIT n timeout`，副本上要读己之写就用 `REPL.TOKEN` 加 `REPL.WAIT`。两者都把 `timeout 0` 理解为“一直等”，硬上限 60 s。

### Linux 内核旋钮

有两个主机级手段，能改变 kevy 脚下那层内核开销。两者都只适合 bench 或单租户场景——动手前先看清取舍。

**Spectre / BHB 缓解。** 在开启缓解的 Linux 6.x 内核上（这是默认状态），每次 syscall 都要为 `clear_bhb_loop` 之类的缓解代码买单。跑小载荷的 `-c 1` 负载时，这是 kevy 全程最大的单项 CPU 消耗。在内核命令行关闭缓解：

```sh
# Add `mitigations=off` to GRUB_CMDLINE_LINUX_DEFAULT, then:
sudo update-grub && sudo reboot
cat /proc/cmdline | grep mitigations
```

只有在不跑任何不可信代码的单租户机器上才可以这么做（没有来自网络的 Lua、没有第三方插件、没有多租户容器）。多租户主机、共享 CI runner，或任何处理不可信用户代码的机器，都不要关。`-c 1` 下的收益在 +10–15% 左右，负载的 pipeline 程度越高，收益越小。

**给 `.text` 段上 hugepages。** kevy 可以对自己的代码段调用 `madvise(MADV_HUGEPAGE)`，让内核用 2 MiB 页而不是 4 KiB 页承载 kevy 二进制的指令，好处是热点分发循环的 iTLB 足迹更小。这在运行时几乎零成本，只要主机的 `/sys/kernel/mm/transparent_hugepage/enabled` 是 `always` 或 `madvise`，就值得开着。代价只有启动时那一次小小的 `madvise` 调用；和 `mitigations=off` 不同，它没有任何安全上的让步。

## Profiling

想让 `perf record` 的火焰图解析出真实符号，要用 `release-perf` profile 构建——优化级别与 `release` 相同，但保留调试行表：

```sh
cargo build --profile release-perf
./target/release-perf/kevy --port 6004 --threads 1 &
KEVY_PID=$!

perf record -F 999 -p $KEVY_PID -g --call-graph=fp -- sleep 30
perf report --stdio | head -60

# Resolve raw addresses for inlined symbols:
addr2line -e ./target/release-perf/kevy -f -i 0x<addr>
```

标准 `release` profile 会剥掉行表，`perf` 只能报出没有符号的裸地址，`addr2line` 也只会返回 `??`。不要对 `release` 二进制做 profile，先用 `release-perf` 重新构建。

要对 `clear_bhb_loop` 等内核侧开销做符号级归因，抓取时把 `fp` 换成 `--call-graph=dwarf`，`addr2line` 的用法不变。dwarf 展栈器更慢，但能正确跨越 syscall 边界。

## 取舍

| 旋钮 | 成本 | 收益 |
|------|------|------|
| `--threads N`（调高） | N 超过负载并行度时，空闲的忙轮询 shard 白烧 CPU | 更高的并发客户端容量 |
| `--threads N`（调低） | 省下一个 shard 份额的跨 shard 转发税 | 稀疏连接负载下少烧 CPU |
| `--accept-shards K` | 监听集中；客户端裸 `connect` 直连时入口更少 | 每轮迭代的开销由每个 accept shard 上更多的连接摊薄 |
| `KEVY_IO_URING=1`（强制） | seccomp 拦截 io_uring 时服务器拒绝启动 | 加固主机上不会静默降级到 epoll |
| `KEVY_IO_URING=0`（强制 epoll） | 放弃 io_uring 的单操作节省 | 可复现的 epoll 基线；可绕开内核回归 |
| `appendfsync always` | 每次写入都阻塞在 `fsync` 上 | 零数据丢失的耐久性 |
| `appendfsync no` | 数据丢失窗口 = page-cache 刷盘间隔 | 最快的写路径 |
| `--no-aof` | 完全没有持久化 | 磁盘 I/O 降到最低；适合副本 / 缓存 |
| 设置 `maxmemory` | 写入可能失败（`noeviction`）或触发驱逐（`allkeys-*`） | 内存占用有上界 |
| 调高 `maxmemory-samples` | 每次驱逐的 CPU 成本上升 | 近似 LRU/LFU 选择驱逐对象更准 |
| Unix-domain socket | 只限本机；安全模型退到文件系统权限 | 绕开 TCP loopback 栈 |
| 设置 `replica_max_staleness_ms` | 落后的副本在追上之前读会失败（`-STALE`） | 读的陈旧度有上界 |
| 设置 `min_replicas_to_write` | 写可用性和副本健康绑在一起（`-NOREPLICAS`） | 不会把数据写进虚空 |
| `mitigations=off` | Spectre / Meltdown / MDS 等缓解全部关闭 | 收回 syscall 路径上的那笔税 |
| 给 `.text` 上 `MADV_HUGEPAGE` | 没有实质成本 | 分发循环的 iTLB 足迹更小 |
| `release-perf` 构建 | 二进制更大（带调试行表） | `perf` 能解析出符号 |

## FAQ

**`--accept-shards` 是不是应该一直设？**

不是。这个旋钮是给稀疏连接负载准备的——连接数除以 shard 数很低、忙轮询循环体摊不开的情形。密集连接负载（比如 1000 个客户端摊在 10 个 shard 上，每 shard 100 个连接）用默认值——所有 shard 都 accept——才是对的，因为把监听均匀铺开能降低 accept 侧的争用。只有确认自己是稀疏连接场景时，才套用 `ceil(conns / 20)`。

**io_uring 一定比 epoll 快吗？**

在 Linux ≥ 5.19 上，只要负载能批量提交，答案是肯定的，而且差距可观。在更老的内核上、在 seccomp 拦截 `io_uring_setup` 的内核上，或者负载每个操作只有一次 syscall、没有批量空间时，差距会收窄。自动检测是正确的默认；只有当你有实测依据，或者是希望高调失败而非静默回退的加固部署，才需要覆盖它。

**生产环境 `appendfsync` 的甜点在哪？**

对绝大多数人都是 `everysec`。它把数据丢失限制在一秒内，把 `fsync` 挪出热路径，对尾延迟几乎没有影响。只有当你的耐久性要求真的是零数据丢失时才用 `always`（并接受尾延迟从此受 NVMe `fsync` 延迟托底）。`no` 只用于纯缓存——那里 AOF 存在的意义只是加快热重启。

**什么时候需要 `MADV_HUGEPAGE`？**

当 `perf` 显示热点分发循环上有 iTLB miss，或者主机的 `/sys/kernel/mm/transparent_hugepage/enabled` 设为 `madvise` 时（这种模式下，除了 kevy 自己没有别的机制会替它开启大页）。在启用了 THP 的 Linux 主机上，这个旋钮没有成本，所以默认立场就是“开着别动”。macOS / BSD 上没有对应机制。

**我的 `perf` 报告全是裸地址，哪里做错了？**

你 profile 的是 `cargo build --release` 出来的二进制。标准 release profile 剥掉了调试行表，`perf` 和 `addr2line` 没有任何可解析的东西。用 `cargo build --profile release-perf` 重新构建，再录一次。
