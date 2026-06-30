# Unix-domain socket(UDS)传输

kevy 提供一个可选的 Unix-domain 流式监听,它讲与 TCP 端口完全一致的 RESP 语义,让同主机客户端彻底跳过 loopback 栈。

## 何时需要

当客户端与服务器在同一台主机上,UDS 就是合适的传输:

- **同主机客户端** —— 应用与 kevy 在一台机器上,或在共享 tmpfs / 已挂载 socket 目录的容器里。
- **延迟敏感负载** —— 低连接数、小载荷,或扇出流水线很高、TCP loopback 往返底线成为瓶颈的场景。
- **容器 sidecar** —— sidecar 与主容器共享 `/run` 或 `/tmp` 卷;socket 文件就是 IPC 句柄,不必分配端口。

跨主机客户端仍需要 TCP —— UDS 受文件系统范围约束,永不离开内核。

## 核心思路

把 `KEVY_UNIX_SOCKET` 设成一个文件系统路径,kevy 就会双绑:TCP 监听保持不变,UDS 监听在同一 shard 运行时上以同一个 RESP2/3 解析器接受连接。任何接受 `unix://` URL 或 `-s <path>` 参数的 RESP 客户端都能用一行配置切过去。UDS 干掉 loopback 的 `rep_movs`、`nft_do_chain` 与 TCP 系统调用路径,因此每操作的底线在所有负载上都明显下降。

## 实际示例

同时启用两种传输:

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6379
```

用 `redis-cli` 走 UDS 连接:

```sh
redis-cli -s /tmp/kevy.sock SET foo bar
# OK
redis-cli -s /tmp/kevy.sock GET foo
# "bar"
```

`:6379` 上的 TCP 仍并行可用 —— 同一份数据、同一组 shard:

```sh
redis-cli -p 6379 GET foo
# "bar"
```

在 Rust 里,内置 client 接受 `unix://` URL:

```rust
let mut conn = kevy_client::Connection::open("unix:///tmp/kevy.sock")?;
conn.set(b"k", b"v")?;
```

## 权限与安全

UDS 的信任边界是**文件系统** —— Unix socket 上没有 RESP 层的 AUTH 或 TLS。能 `open(2)` 这个 socket 文件的人,就能下任何命令,包括 `FLUSHALL`。

- **socket 文件归属。** kevy 以服务器运行身份创建 socket。启动后用 `chown` / `chgrp` 调整,或者以你想拥有 socket 的身份去跑 kevy。
- **权限位。** socket 默认以宽松权限位创建,以便同地客户端进程能连接。要收紧,请把 socket 放进权限严格的目录里 —— 例如 `/run/kevy/` 归 `kevy` 组、`0750`,使得只有组成员能 `connect(2)`。目录权限把守 socket inode 自身的访问。
- **tmpfs vs 磁盘。** `/tmp` 和 `/run` 在大多数 Linux 发行版上都是 tmpfs,对 socket 而言再合适不过(连接时无磁盘 IO)。真实文件系统上的持久路径也可以 —— inode 只是会合点,数据从不落盘。
- **信任域。** 把任何在 socket 路径上有读写权限的账号当作完全已认证。如果你需要按客户端区分身份,这事得在 kevy 之上来做(sidecar 代理、内核 LSM、命名空间隔离)。

## 服务器配置旋钮

| Env 变量 | CLI 标志 | 默认 | 效果 |
|---|---|---|---|
| `KEVY_UNIX_SOCKET` | (目前仅 env)| 未设置 | 要绑定的文件系统路径。不设则只走 TCP。 |
| `KEVY_BIND` | `--bind` | `127.0.0.1` | TCP 绑定地址;UDS 绑定独立。 |
| `--port` | `--port` | `6379` | TCP 端口;设置 UDS 时仍然绑定 TCP。 |

注意:

- **路径必须不存在。** 如果 `KEVY_UNIX_SOCKET` 已指向某个文件,kevy 拒绝启动 —— 它不会覆盖一个不是自己创建的路径。重启前清理(`rm -f /tmp/kevy.sock`)或使用每次运行不同的路径(`/run/kevy/$(date +%s).sock`)。这是故意的:静默 unlink 会让配置错误的 kevy 偷走别的服务的 socket。
- **设置环境变量后双绑总开。** 没有"只 UDS"模式 —— TCP 监听仍然在。要禁止 TCP,把它绑到一个你控制的回环地址并用防火墙挡住。
- **shard 0 拥有 accept 循环。** 接受到的连接被分发到已有的 per-shard 运行时,因此 `--threads` 仍控制 socket 后面工作负载的并行度。
- **io_uring 路径。** 在 Linux 上加 `KEVY_IO_URING=1` 时,UDS accept 作为 multishot accept SQE 走与 TCP 同一个 io_uring 实例 —— 无额外 reactor 成本。`TCP_NODELAY` 在 UDS 上不设(它不是 IP socket)。

## 取舍

UDS vs 同一 kevy 二进制的 TCP loopback:

| 方面 | UDS | TCP loopback |
|---|---|---|
| 每操作底线 | 更低(无 IP/校验和/端口/NAGLE)| 较高 |
| 触及范围 | 仅同主机 | 任意主机 |
| 身份 | 文件系统权限 | 端口 + 绑定地址 + AUTH |
| 生命周期 | 磁盘上的 socket 文件;重启需清理 | 端口生命周期由内核管理 |
| 可观测 | `lsof` / `ss -xl` | `ss -tln`、`netstat`、`tcpdump` |
| 客户端配置 | `unix:///path` 或 `-s /path` | `host:port` |

吞吐收益取决于负载形态 —— 小载荷、低连接的格子收益最大(loopback 每操作税在它们上面占主导);CPU 饱和的格子收益较小(传输不是瓶颈)。实测数据见 [bench/REPORT.md](https://github.com/goliajp/kevy/blob/master/bench/REPORT.md)。

## FAQ

### 我能同时绑 UDS 与 TCP 吗?

能 —— 这就是唯一模式。设置 `KEVY_UNIX_SOCKET` 会加 UDS 监听;TCP 监听保持原样不变。按客户端各取所需。

### 服务器拒绝启动 —— "socket exists"?

是故意的。kevy 不会 `unlink` 一个不是自己创建的路径,因为这会让配置错误的运行静默偷走别的服务的 socket。要么在重启前删掉那个旧文件(`rm -f /tmp/kevy.sock`),要么用每次运行不同的路径,例如 `/run/kevy/$(uuidgen).sock`。如果是 kevy 崩溃留下的文件,手工删除是安全的。

### UDS 比 TCP loopback 快多少?

在所有负载上都明显更快,因为 UDS 跳过了整段 IP 路径:没有校验和、没有 netfilter 链(`nft_do_chain`)、没有走 loopback 的 `rep_movs`、没有每包 ACK 往返。具体比率取决于 loopback 开销在每操作预算里的占比 —— 单连接小载荷负载提升最大;CPU 受限的流水线格子提升较小。用 `redis-benchmark -s /tmp/kevy.sock` vs `-h 127.0.0.1` 在你的负载上量。

### 我的客户端库能用 UDS 吗?

大多数能。`redis-cli` 与 `redis-benchmark` 接受 `-s <path>`。ioredis、node-redis、redis-py、redis-rb、go-redis、lettuce、jedis,以及内置的 [kevy-client](https://github.com/goliajp/kevy/tree/master/crates/kevy-client) / [kevy-client-async](https://github.com/goliajp/kevy/tree/master/crates/kevy-client-async) 都接受 `unix:///path` URL 或一个明确的 socket-path 选项。具体键名请查你 driver 的连接选项文档。

### 如果我所有客户端都在同主机,能不能完全丢掉 TCP?

可以,但不必。让 TCP 绑在 `127.0.0.1` 上没人连接时不花什么成本,且在客户端 UDS 路径配错时还能作回退。常见部署是"热客户端走 UDS,`redis-cli` 调试走 TCP"。
