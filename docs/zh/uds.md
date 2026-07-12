# Unix-domain socket（UDS）传输

kevy 提供一个可选的 Unix-domain 流式监听，RESP 语义与 TCP 端口完全一致，让同主机客户端彻底绕开 loopback 栈。

## 什么时候需要它

客户端和服务器同在一台主机上时，UDS 就是合适的传输：

- **同主机客户端**——应用和 kevy 跑在同一台机器上，或者在共享 tmpfs / 挂载了 socket 目录的容器里。
- **延迟敏感负载**——连接数少、载荷小，或者 pipeline 扇出很大、TCP loopback 的往返开销下限成了瓶颈。
- **容器 sidecar**——sidecar 与主容器共享 `/run` 或 `/tmp` 卷；socket 文件就是 IPC 句柄，不用分配端口。

跨主机的客户端仍然要走 TCP——UDS 以文件系统为作用域，永远出不了内核。

## 核心思路

把 `KEVY_UNIX_SOCKET` 设成一个文件系统路径，kevy 就会双绑定：TCP 监听原样保留，UDS 监听跑在同一套 shard 运行时上，用同一个 RESP2/3 解析器接受连接。任何支持 `unix://` URL 或 `-s <path>` 参数的 RESP 客户端，改一行配置就能切过去。UDS 省掉了 loopback 的 `rep_movs`、`nft_do_chain` 和整条 TCP 系统调用路径，所以单次操作的开销下限在所有负载上都有明显下降。

## 实例

同时启用两种传输：

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6379
```

用 `redis-cli` 走 UDS 连接：

```sh
redis-cli -s /tmp/kevy.sock SET foo bar
# OK
redis-cli -s /tmp/kevy.sock GET foo
# "bar"
```

`:6379` 上的 TCP 依然并行可用——同一份数据、同一组 shard：

```sh
redis-cli -p 6379 GET foo
# "bar"
```

内置的 Rust 客户端（`kevy-client` / `kevy-client-async`）讲 `tcp://` / `kevy://` / `redis://` 加上进程内的 `mem://`、`file:///` 两种 scheme——它们不接受 `unix://` URL。在 Rust 里，同宿主客户端要么走 TCP loopback 连接，要么当它与服务端同在一个*进程*里时，用 embedded 后端（`file:///` / `mem://`）彻底跳过 socket——那比 UDS 所能达到的还要快。UDS 服务的是其他语言或生态 driver 的跨进程同宿主客户端。

## 权限与安全

UDS 的信任边界是**文件系统**——Unix socket 上没有 RESP 层的 AUTH 或 TLS。谁能 `open(2)` 这个 socket 文件，谁就能下任何命令，包括 `FLUSHALL`。

- **socket 文件归属。**kevy 以服务器的运行身份创建 socket。启动后用 `chown` / `chgrp` 调整，或者干脆用你希望拥有 socket 的身份来跑 kevy。
- **权限位。**socket 默认以宽松权限位创建，好让同机的客户端进程能连上。要收紧，把 socket 放进权限严格的目录——比如 `/run/kevy/` 归 `kevy` 组、权限 `0750`，这样只有组成员能 `connect(2)`。目录权限就是 socket inode 的访问闸门。
- **tmpfs 还是磁盘。**多数 Linux 发行版上 `/tmp` 和 `/run` 都是 tmpfs，放 socket 再合适不过（连接时没有磁盘 IO）。放在真实文件系统的持久路径上也行——inode 只是个会合点，数据从不落盘。
- **信任域。**凡是对 socket 路径有读写权限的账号，都要当成完全通过认证来对待。需要区分客户端身份的话，得在 kevy 之上解决（sidecar 代理、内核 LSM、命名空间隔离）。

## 服务器配置旋钮

| Env 变量 | CLI 标志 | 默认 | 效果 |
|---|---|---|---|
| `KEVY_UNIX_SOCKET` | （目前仅 env） | 未设置 | 要绑定的文件系统路径。不设则只走 TCP。 |
| `KEVY_BIND` | `--bind` | `127.0.0.1` | TCP 绑定地址；UDS 绑定与之独立。 |
| `--port` | `--port` | `6379` | TCP 端口；设了 UDS 之后 TCP 照样绑定。 |

注意事项：

- **路径必须事先不存在。**如果 `KEVY_UNIX_SOCKET` 指向的文件已经存在，kevy 拒绝启动——它不会覆盖一个不是自己创建的路径。重启前清理（`rm -f /tmp/kevy.sock`），或者每次运行用不同路径（`/run/kevy/$(date +%s).sock`）。这是刻意设计：静默 unlink 会让配置错误的 kevy 偷走别的服务的 socket。
- **设了环境变量就必然双绑定。**没有“只 UDS”模式——TCP 监听始终在。想禁掉 TCP，把它绑到一个你可控的回环地址，再用防火墙挡住。
- **accept 循环归 shard 0。**接受的连接会分发到既有的 per-shard 运行时，所以 `--threads` 依然控制 socket 背后工作负载的并行度。
- **io_uring 路径。**Linux 上加 `KEVY_IO_URING=1` 时，UDS 的 accept 以 multishot accept SQE 的形式，走与 TCP 同一个 io_uring 实例——没有额外的 reactor 成本。UDS 上不设 `TCP_NODELAY`（它不是 IP socket）。

## 取舍

同一个 kevy 二进制上，UDS 对比 TCP loopback：

| 方面 | UDS | TCP loopback |
|---|---|---|
| 单操作开销下限 | 更低（没有 IP/校验和/端口/NAGLE） | 较高 |
| 可达范围 | 仅同主机 | 任意主机 |
| 身份 | 文件系统权限 | 端口 + 绑定地址 + AUTH |
| 生命周期 | 磁盘上的 socket 文件；重启要清理 | 端口生命周期由内核管理 |
| 可观测性 | `lsof` / `ss -xl` | `ss -tln`、`netstat`、`tcpdump` |
| 客户端配置 | `unix:///path` 或 `-s /path` | `host:port` |

吞吐收益取决于负载形态——小载荷、低连接数的格子收益最大（loopback 的每操作税在它们身上占大头）；CPU 已饱和的格子收益较小（瓶颈本来就不在传输）。实测数据见 [bench/REPORT.md](https://github.com/goliajp/kevy/blob/develop/bench/REPORT.md)。

## FAQ

### UDS 和 TCP 能同时绑吗？

能——而且这是唯一的模式。设置 `KEVY_UNIX_SOCKET` 只是加一个 UDS 监听；TCP 监听原样不动。每个客户端各走各的就好。

### 服务器拒绝启动，报"socket exists"？

这是刻意的。kevy 不会 `unlink` 一个不是自己创建的路径，否则一次配置错误的运行就能悄悄偷走别的服务的 socket。要么重启前删掉旧文件（`rm -f /tmp/kevy.sock`），要么每次运行用不同路径，比如 `/run/kevy/$(uuidgen).sock`。如果文件是 kevy 崩溃后残留的，手工删掉是安全的。

### UDS 比 TCP loopback 快多少？

所有负载上都明显更快，因为 UDS 跳过了整条 IP 路径：没有校验和、没有 netfilter 链（`nft_do_chain`）、没有走 loopback 的 `rep_movs`、没有每包的 ACK 往返。具体倍率取决于 loopback 开销在每操作预算里占多大比例——单连接、小载荷的负载提升最大；CPU 受限的 pipeline 格子提升较小。用 `redis-benchmark -s /tmp/kevy.sock` 对比 `-h 127.0.0.1`，在你自己的负载上量一下。

### 我的客户端库支持 UDS 吗？

生态 driver 大多支持。`redis-cli` 和 `redis-benchmark` 接受 `-s <path>`。ioredis、node-redis、redis-py、redis-rb、go-redis、lettuce、jedis 都接受 `unix:///path` URL 或显式的 socket-path 选项——具体键名查你所用 driver 的连接选项文档。内置的 [kevy-client](https://github.com/goliajp/kevy/tree/develop/crates/kevy-client) / [kevy-client-async](https://github.com/goliajp/kevy/tree/develop/crates/kevy-client-async) **不**讲 UDS：同进程的 Rust 客户端用 embedded 的 `file:///` / `mem://` 后端胜过任何 socket，跨进程则走 TCP。

### 客户端全在同一台主机上，能不能干脆不要 TCP？

可以，但没必要。TCP 绑在 `127.0.0.1` 上，没人连就没有成本，而且客户端 UDS 路径配错时还能当回退。常见的部署是“热点客户端走 UDS，`redis-cli` 调试走 TCP”。
