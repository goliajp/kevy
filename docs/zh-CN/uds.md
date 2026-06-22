# Unix-domain socket (UDS) 传输

v1.25 加了 opt-in 的 Unix-domain stream socket listener —— kevy 对
应 valkey/redis `unixsocket` 配置的等价物。对本机客户端(server
进程同主机、同信任域),UDS 整段跳过 TCP loopback 栈:无 IP 头、
无 checksum、无端口查找、无 NAGLE/ACK 来回。线协议是 RESP2 / 3
逐字节兼容,客户端只换一个 URL 就过去了。

## 何时使用

UDS 适用于**同时满足**:

1. 客户端跟 server 在**同一主机**(容器之间挂同一 tmpfs / host volume
   也算)。
2. 你瓶颈在 per-syscall 网络开销 —— 小 payload、高连接数、单 shard
   server,或 `-c1` 类负载付完整的 per-op RTT。
3. 信任域是**主机文件系统**(UDS 权限即文件系统权限;kevy 和 valkey
   都没有 AUTH/TLS)。

UDS **不替代** TCP 的场景:

- 客户端在另一个容器**没挂共享 socket** —— `/tmp/kevy.sock` 路径要
  对两边都可见。
- 你需要网络可达性 —— 远端客户端只能走 TCP loopback / 远端 TCP
  (kevy 是单 DC,不为公网设计)。
- 负载是 `-c50 -P16` pipelined 且已饱和 server CPU —— UDS 在这种
  workload 上能多挖几个百分点,但杠杆不在传输层。

为何 kevy 的 UDS 提升比 valkey 大,见 [`bench/REPORT.md`](../../bench/REPORT.md)
的 Phase A 分解叙事。

## Server 配置

启动前把 `KEVY_UNIX_SOCKET` 指向一个文件路径。server 会在 TCP
listener 之外**同时绑 UDS** —— 两个 listener 并行 accept,客户端
自己挑用哪一个:

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
```

行为:

- bind 前先 `unlink` 路径(前一次崩溃留下的 stale socket 会被自动
  清理 —— 跟 valkey/redis 一致)。
- bind 后 `chmod 0777`(任何本机用户都能连;要做 per-user 控制就
  靠包含目录的权限收紧)。
- 只有 **shard 0** 持有 UDS listener;accept 后的连接派发到现有
  per-shard runtime,所以 `--threads` 设置仍然控制 socket 后端的
  并行度。
- Linux 上配 `KEVY_IO_URING=1`,UDS accept loop 跑成 multishot
  accept SQE,在同一个 io_uring 实例里 —— 没有额外 reactor 成本。
  UDS 不是 IP socket,所以跳过 TCP_NODELAY。
- 空 / 未设 `KEVY_UNIX_SOCKET` = 仅 TCP(v1.24 及之前行为不变)。

CLI / TOML 等价项计划在 v1.26 加上;目前只有环境变量这一个旋钮。

## Client 配置

任何接受 Unix-socket 选项的 Redis/RESP 客户端开箱即用 —— 同 RESP2/3
帧格式。

`redis-cli` / `redis-benchmark`(`-s` 标志):

```sh
redis-cli -s /tmp/kevy.sock SET foo bar
redis-cli -s /tmp/kevy.sock GET foo
redis-benchmark -s /tmp/kevy.sock -t set,get -n 100000 -c 50 -P 16
```

[`kevy-client`](../../crates/kevy-client) 和
[`kevy-client-async`](../../crates/kevy-client-async) 接受 `unix://`
URL:

```rust
let mut conn = kevy_client::Connection::open("unix:///tmp/kevy.sock")?;
conn.set(b"k", b"v")?;
```

valkey / redis 对比配置(他们的 `unixsocket` 指令):

```sh
valkey-server --unixsocket /tmp/valkey.sock --unixsocketperm 777 \
              --io-threads 10
redis-server  --unixsocket /tmp/redis.sock  --unixsocketperm 777
```

## Bench 数字

Precision bench,n=1 M × 10 runs,2σ 过滤均值,CI95 < 1 % 全表。lx64、
`mitigations=off`,kevy `--threads 1`(单 shard),valkey
`--io-threads 10`。用
[`bench/v125-precision-uds.sh`](../../bench/v125-precision-uds.sh) 复现。

| 工作负载 | kevy 1.25 (UDS) | valkey 9.1 (UDS) | kevy / valkey |
|----------|----------------:|-----------------:|--------------:|
| -c1 SET | **166 k/s** | 96 k/s | **1.73×** |
| -c1 GET | **168 k/s** | 106 k/s | **1.59×** |
| -c50 -P1 SET | 339 k/s | 334 k/s | 打平(per-syscall 地板) |
| -c50 -P1 GET | 337 k/s | 332 k/s | 打平(per-syscall 地板) |
| **-c50 -P16 SET** | **4.11 M/s** | 1.75 M/s | **2.35×** |
| **-c50 -P16 GET** | **4.35 M/s** | 3.42 M/s | **1.27×** |
| -c100 -P1 SET | 331 k/s | 326 k/s | 打平 |
| -c100 -P1 GET | 335 k/s | 327 k/s | 打平(1.02×) |

UDS vs TCP for kevy(同 server、同 bench,只换传输层):

| 工作负载 | TCP rps | UDS rps | UDS / TCP |
|----------|--------:|--------:|----------:|
| -c1 SET | 94.7 k | 166 k | **1.76×** |
| -c1 GET | 97.3 k | 168 k | **1.73×** |
| -c50 -P1 | 192 k | 339 k | **1.77×** |
| -c50 -P16 SET | 2.59 M | 4.11 M | **1.59×** |
| -c50 -P16 GET | 2.67 M | 4.35 M | **1.63×** |

为何 kevy 的 UDS 提升比 valkey 大:valkey 的热路径更 CPU-bound
(`processCommand` / `addReply` 里的 per-op 工作),它的 TCP 天
花板已经低于传输 RTT 地板 —— 去掉 loopback 没给 valkey 多少新
余地。kevy 的热路径足够轻,所以 TCP RTT 地板是 `-c50 -P16` 上的
约束;UDS 解除约束后 server 比 loadgen 跑得还快。c=50/100 -P1
的打平在 UDS 上仍然打平 —— 两个 server 都被 per-syscall round-trip
地板(~3 µs × 50 conn)卡住,跟传输层无关。

## 安全注意

- **文件权限 = AUTH 等价物。** UDS 没有原生身份验证;能 `open(2)`
  这个 socket 文件的人就能下任意命令(包括 `FLUSHALL`)。kevy 默认
  `chmod 0777` 跟 valkey/redis 一致;要收紧就把 socket 放进受限
  权限的目录,例如 `/run/kevy/kevy.sock` 属 `kevy` 组。
- **崩溃后 stale socket。** kevy bind 前会 `unlink`,所以前次崩溃
  留下的文件不会阻塞启动。两个 kevy 实例指同一路径,后启动的赢 —
  前者的客户端下次写时会拿到 `EPIPE`。
- **非远端。** UDS 是 host-local。跨主机客户端只能走 TCP(kevy
  仍是单 DC、无 AUTH/TLS —— 见
  [`README.zh-CN.md`](../../README.zh-CN.md))。

## 复现

```sh
ssh lx64
bash /path/to/kevy/bench/v125-precision-uds.sh
```

precision harness 编同一 kevy binary,逐个起 kevy 和 valkey,跑
`redis-benchmark -s <sock>` 10 次 × n=1 M,打印过滤均值 + CI95。
配套的 smoke
[`bench/v125-uds-smoke.sh`](../../bench/v125-uds-smoke.sh)(14 组、
39 断言,覆盖 SET/GET、所有 collection、INCR/APPEND、大值、SETEX、
MSET、pipelined DBSIZE、pub/sub、FLUSHALL、INFO)确认 UDS 是
wire-equivalent 的 —— server 端走同一段代码,只是 accept SQE 不同。
