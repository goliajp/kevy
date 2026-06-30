# 调优 kevy

一份关于会改变 kevy 每操作开销的运行时旋钮参考 —— CPU 布局、reactor 选择、持久化、内存上限、网络传输,以及若干 Linux 侧的杠杆。

## 何时需要

下列情况下查阅本页:

- 一个 benchmark 显示 kevy 落在你的吞吐或延迟目标之下,你想知道下一个该转什么旋钮。
- 你正在部署 kevy 的主机,默认值(TCP loopback、io_uring 自动检测、`appendfsync everysec`、无 `maxmemory`)与负载不匹配 —— 例如稀疏连接服务、NVMe 支撑的耐久要求,或限内存的缓存层。
- 你正用 `perf` 给 kevy 做 profiling,需要保留调试行表的构建 profile。

如果你只是在笔记本上启动 kevy 看着数还行,就不需要本页。默认值的目标是"对各种负载都还过得去"。

## 核心思路

kevy 是 thread-per-core 服务器:每个 OS 线程一个 shard、按 CRC16 hashtag 分区、shared-nothing 键空间、每 shard 一个 busy-poll reactor。默认值的目标是"对各种负载都不错";调优意味着把 shard 数、reactor、与持久化策略匹配到**你的性能数据真正显示的瓶颈**上。不要预先翻旋钮。测量,识别成本,然后一次改一个变量。

## 调优 playbook

### CPU 与 shard

| 旋钮 | 位置 | 默认 | 效果 |
|------|------|------|------|
| `--threads N` / `KEVY_THREADS` | CLI / env | 在线核数 | shard 数;每 shard 一个 OS 线程 |
| `--accept-shards K` | CLI | 所有 shard 接受 | 只有前 K 个 shard 绑监听,其余只计算 |
| CPU 绑定 | `taskset` / `numactl` | 无 | 把 shard 锁到固定核集合 |

**挑选 `--threads`。** 把它设到负载里实际存在的并行度。单客户端流水线 benchmark(`-c 1 -P 16`)只压满一个 shard;此时设 `--threads 10` 会让另外九个 shard 给没事干的事情 busy-poll,还顺便从 shard 0 偷 cache line。对真实多客户端负载,从 `min(cores, 期望并发客户端 / 4)` 开始,然后测。

**挑选 `--accept-shards`。** 当连接数对 shard 数的比值很低(稀疏连接 —— 比如 50 个客户端摊到 10 个 shard = 每 shard 5 连)时,每轮 busy-poll 开销摊不开,吞吐下降。经验法则是 `ceil(conns / 20)` —— 50 conns 设 `--accept-shards 3`,让 3 个监听 shard 各负担约 17 个连接,其余 shard 留作只计算,通过内部 dispatcher 仍然接收跨 shard 工作。实测甜区比单点估算更宽;完整扫描以及何时跨 shard 跳的税会盖过 accept 集中收益的讨论见 [docs/accept-shards.md](https://github.com/goliajp/kevy/blob/develop/docs/accept-shards.md)。

**CPU 绑定。** 在 benchmark 或单租户主机上,把 kevy 绑到固定核集合可以让 NIC IRQ → softirq → 用户线程路径留在同一片 L1/L2 上:

```sh
taskset -c 0-9 kevy --port 6004 --threads 10
```

如果客户端跑在同台机器上,把服务器与客户端绑到**互不相交**的核范围(服务器 `0-9`、客户端 `10-15`)。共享核会带来调度乒乓,把任何 reactor 收益淹没。

### Reactor 选择

| 平台 | 默认 | 覆盖 |
|------|------|------|
| Linux ≥ 5.19 | io_uring(自动检测)| `KEVY_IO_URING=0` 强制 epoll;`KEVY_IO_URING=1` 要求 io_uring,若 `io_uring_setup` 被 seccomp 拦截则大声退出 |
| macOS / *BSD | kqueue | 不可配 |
| 较旧 Linux | epoll | 不适用 |

Linux 自动检测在启动时跑 `io_uring_setup`;如果该 syscall 被拦(seccomp profile、锁定容器),kevy 静默回退到 epoll。在希望*响亮失败*而不是静默降级的强化部署里,把 `KEVY_IO_URING=1` 设上,服务器在 io_uring 不可用时就拒绝启动。反过来,要为可复现的 epoll-vs-io_uring benchmark 把 io_uring 从画面里拿掉,或绕开某个内核回归,设 `KEVY_IO_URING=0`。

```sh
KEVY_IO_URING=1 kevy --port 6004   # 要求 io_uring,被拦则退出
KEVY_IO_URING=0 kevy --port 6004   # 强制 epoll
```

### 持久化

AOF 策略由 `appendfsync` 控制(配置文件或 `CONFIG SET`)。三个值与 Redis 语义匹配:

| `appendfsync` | 耐久 | 代价 |
|---------------|------|------|
| `always` | 每次写入回复前 `fsync` | 延迟最高;受 NVMe sync 延迟约束 |
| `everysec`(默认)| 后台线程每秒 `fsync` | 数据丢失窗口 1 秒;热路径近零成本 |
| `no` | 永不 `fsync`;内核按自己日程刷 | 最快;数据丢失窗口 = page-cache 刷写间隔 |

`everysec` 的后台 `fsync` 跑在 shard 热路径之外的专用 bio 线程,因此 shard 尾延迟不与磁盘延迟耦合。对纯缓存或只读副本,也可考虑直接用 `--no-aof` 关掉 AOF(根本不写 AOF 文件,甚至不缓冲)。

### 内存

| 旋钮 | 默认 | 作用 |
|------|------|------|
| `maxmemory` | 无限 | 字节硬上限;达到上限后启动驱逐策略 |
| `maxmemory-policy` | `noeviction` | 上限时丢哪些键 |
| `maxmemory-samples` | 5 | 近似 LRU/LFU 策略的采样大小 |

驱逐策略与 Redis 一致:`noeviction`、`allkeys-lru`、`allkeys-lfu`、`allkeys-random`、`volatile-lru`、`volatile-lfu`、`volatile-random`、`volatile-ttl`。`noeviction` 让上限到达后的写以 OOM 失败,是主存储的安全默认;`allkeys-*` 策略适合任何键都可丢弃的缓存层。

`maxmemory-samples` 是近似策略的"质量 vs 成本"刻度 —— 采更多键得到更接近真 LRU/LFU 的近似,代价是每次驱逐的 CPU。默认 5 对大部分缓存负载够用;如果你看到驱逐挑了糟糕的牺牲品,提到 10;只有驱逐本身出现在 profile 里才降到 3。

### 网络

默认传输是 TCP。客户端在同主机时,切到 Unix-domain socket,彻底跳过 loopback TCP 栈:

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
redis-cli -s /tmp/kevy.sock SET foo bar
```

服务器双绑:TCP 留给远程客户端,UDS 处理本地。相同 RESP 语义、相同 shard 运行时。本地客户端负载的提升很大(在小载荷尺寸下 loopback TCP 路径是主要成本);完整数字、权限模型与不适用 UDS 的情形见 [docs/uds.md](uds.md)。

**绑定地址警告。** kevy 目前没有 AUTH 也没有 TLS。绑到非 loopback 地址(`--bind 0.0.0.0` 或任何公网接口)会打印启动警告,因为网络上任何东西都能下命令。把 kevy 跑在私网边界之内,或在前面放一个做认证的代理。

### Linux 内核旋钮

两个主机级杠杆能挪动 kevy 之下的内核底线。两者都只适合 benchmark / 单租户场景 —— 应用前先读取舍。

**Spectre / BHB 缓解。** Linux 6.x 内核默认启用缓解后,每次 syscall 都为 `clear_bhb_loop` 及其同伴付钱。在小载荷的 `-c 1` 负载下,这是 kevy 单次运行最大的 CPU 消耗。从内核命令行关掉缓解:

```sh
# 在 GRUB_CMDLINE_LINUX_DEFAULT 里加 `mitigations=off`,然后:
sudo update-grub && sudo reboot
cat /proc/cmdline | grep mitigations
```

只在没有不可信代码运行的单租户机器上可接受(没有线协议过来的 Lua、没有第三方插件、没有多租户容器)。不要应用到多租户主机、共享 CI runner,或任何处理不可信用户代码的地方。`-c 1` 上的提升在 +10–15% 范围内,流水线越多提升越小。

**`.text` 段大页。** kevy 可以对自己的代码段调用 `madvise(MADV_HUGEPAGE)`,让内核用 2 MiB 页而不是 4 KiB 页来支撑 kevy 二进制的指令。收益是热分派循环的 iTLB 占用更小。运行时基本没成本,值得在 `/sys/kernel/mm/transparent_hugepage/enabled` 为 `always` 或 `madvise` 的 Linux 主机上启用。代价就是启动时一次小的 `madvise` 调用;与 `mitigations=off` 不同,没有安全代价。

## Profiling

要让 `perf record` 火焰图能解析到真正符号,请用 `release-perf` profile 构建 —— 与 `release` 同等优化级别,但保留调试行表:

```sh
cargo build --profile release-perf
./target/release-perf/kevy --port 6004 --threads 1 &
KEVY_PID=$!

perf record -F 999 -p $KEVY_PID -g --call-graph=fp -- sleep 30
perf report --stdio | head -60

# 为内联符号解析原始地址:
addr2line -e ./target/release-perf/kevy -f -i 0x<addr>
```

标准 `release` profile 会剥掉行表,`perf` 报告就只剩原始地址、`addr2line` 只返回 `??`。不要给 `release` 二进制做 profile;先用 `release-perf` 重编。

要做 `clear_bhb_loop` 等内核侧成本的符号级归因,用 `--call-graph=dwarf` 抓取(不是 `fp`),其余流程相同。dwarf unwinder 较慢,但能正确穿过 syscall 边界。

## 取舍

| 旋钮 | 成本 | 收益 |
|------|------|------|
| `--threads N`(调高)| N 大于负载并行度时,空闲 busy-poll shard 浪费 CPU | 更多并发客户端容量 |
| `--threads N`(调低)| 少了一个 shard 的跨 shard 跳税 | 稀疏连接负载下少浪费 CPU |
| `--accept-shards K` | 监听集中;客户端用原始 `connect` 时入口更少 | 每轮开销在每个接受 shard 上的更多连接上摊开 |
| `KEVY_IO_URING=1`(强制)| seccomp 拦截 io_uring 时服务器拒启 | 强化主机上不再静默降级到 epoll |
| `KEVY_IO_URING=0`(强制 epoll)| 放弃 io_uring 的每操作节省 | 可复现 epoll 基线;绕开内核回归 |
| `appendfsync always` | 每次写都阻塞在 `fsync` | 零数据丢失耐久 |
| `appendfsync no` | 数据丢失窗口 = page-cache 刷写间隔 | 最快写路径 |
| `--no-aof` | 完全无持久化 | 最小磁盘 IO;副本 / 缓存有用 |
| 设 `maxmemory` | 写入可能失败(`noeviction`)或触发驱逐(`allkeys-*`)| 内存占用有界 |
| 提高 `maxmemory-samples` | 每次驱逐 CPU 上升 | 近似 LRU/LFU 选牺牲品更好 |
| Unix-domain socket | 仅本地;文件系统权限的安全模型 | 跳过 TCP loopback 栈 |
| `mitigations=off` | Spectre / Meltdown / MDS / 等缓解全关 | 把 syscall 路径税收回来 |
| 对 `.text` 上 `MADV_HUGEPAGE` | 无意义成本 | 分派循环 iTLB 占用更小 |
| `release-perf` 构建 | 二进制更大(带调试行表)| `perf` 能解析到符号 |

## FAQ

**我是不是应该总是设 `--accept-shards`?**

不是。这个旋钮存在是为稀疏连接负载,即 conns/shards 低、busy-poll 摊不开的情形。对密集连接负载(比如 1000 个客户端摊到 10 个 shard = 每 shard 100 连),默认 —— 每 shard 都接受 —— 才对,因为均匀摊开监听会降低 accept 侧争用。只有真有稀疏连接情况时才用 `ceil(conns / 20)`。

**io_uring 是不是永远比 epoll 快?**

在 Linux ≥ 5.19 上,对会批量提交的负载,是,显著。在更老的内核、拦截 `io_uring_setup` 的 seccomp 过滤、或每操作只有一次 syscall 且没有批量机会的负载上,差距收窄。自动检测就是合适默认;只有在你有实测理由或需要响亮失败的强化部署时才覆盖。

**`appendfsync` 的生产甜区在哪?**

`everysec` 对几乎所有人都适合。它把数据丢失界定在一秒,把 `fsync` 移出热路径,对尾延迟接近零冲击。只有当你的耐久需求真的要求零丢失时才用 `always`(并接受 NVMe `fsync` 延迟现在就是尾延迟上限)。`no` 只用于纯缓存,那里 AOF 只为热重启速度而存在。

**什么时候需要 `MADV_HUGEPAGE`?**

当 `perf` 在热分派循环上显示 iTLB miss,或主机的 `/sys/kernel/mm/transparent_hugepage/enabled` 为 `madvise`(那种情况下只有 kevy 自己 opt-in)。这是 Linux 上启用 THP 的主机上的无成本旋钮,所以默认立场是"留着开"。macOS / BSD 没有等价物。

**我的 `perf` 报告全是原始地址,做错了什么?**

你 profile 了 `cargo build --release` 的二进制。标准 release profile 剥掉了调试行表,所以 `perf` 和 `addr2line` 都没东西能解析。用 `cargo build --profile release-perf` 重编再重录。
