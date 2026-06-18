# kevy

[English](README.md) · **简体中文** · [日本語](README.ja.md)

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)

纯 Rust、**零依赖**、兼容 Redis 的键值存储 —— 既可作为独立服务器，也可作为
嵌入式库使用，以硬件允许的最快速度运行。

kevy 使用 Redis 线协议（RESP2），因此 `redis-cli`、`valkey-cli` 以及所有
Redis 客户端库都能**无需改动**直接连接。底层引擎是完全用 Rust 编写的现代
thread-per-core、shared-nothing 架构 —— 唯一触及的 C 是无法回避的操作系统
系统调用边界。

```sh
cargo run -p kevy --bin kevy --release      # 仅 loopback，AOF 开启，端口 6004
redis-cli -p 6004 SET hello world
```

## 为什么选 kevy

- **快** —— 高并发吞吐达 valkey 9.1 的 2.3–2.7×，pub/sub 扇出 2.7×，嵌入式
  单核约 1800 万 ops/s（数据见下文）。
- **占用极小** —— 768 KB 的服务器二进制，启动后内存不到 5 MB。容器 sidecar、
  小型 VM、边缘设备都装得下。
- **架构先进** —— thread-per-core、shared-nothing，热路径无锁，Linux 上用
  io_uring。没有全局锁，没有 GIL 式瓶颈。
- **无供应链风险** —— 零 crates.io 依赖。整棵依赖树只有 `std` 加 kevy 自己的
  crate；唯一的 C 是操作系统系统调用边界，在单个 crate 里手写绑定。除了 kevy
  本身，没有别的要审计。
- **直接兼容** —— RESP2 线协议，与 valkey 9.1 达成 94 条命令对等，回复逐字节
  核对。现有客户端和工具直接可用。
- **可嵌入** —— `kevy-store` 是一个普通的 Rust 库：无网络、无运行时，还能为
  `wasm32` 构建。同一套引擎，跑在你的进程里。
- **资源自适应** —— 内存无上限时满速运行，受限时优雅降级，到边界时大声拒绝
  而非静默损坏（[详见](#资源自适应设计)）。

关于适用范围我们如实说明：kevy 是**单机**的 —— 不做复制、集群、AUTH/TLS，
也不直接暴露到公网（见[何时使用 kevy](#何时使用-kevy)）。

## 性能

下列所有数据都在一台**裸金属 Intel Core i7-10700K**（8 核 / 16 线程，
3.8 GHz 基频 / 5.1 GHz 睿频）、62 GB 内存、Linux 6.12.90 上测得，全内存。
每项基准都可用 [`bench/`](bench/) 里的脚本复现；完整方法与注意事项见
[`bench/REPORT.md`](bench/REPORT.md)。

### 服务器吞吐（走网络）

> 超越 valkey 9.1 只是下限，不是目标 —— kevy 瞄准的是硬件天花板。

`redis-benchmark`，每个服务端 pin 到 0–9 核、客户端用独立核，且各自单独运行。
每个引擎都用其最快配置（kevy：-c50 用 io_uring，-c1 用 epoll；valkey/redis：
io-threads）：

| 负载 | kevy | valkey 9.1 | redis 7.4 |
|------|-----:|-----------:|----------:|
| **-c50 -P16 GET** | **4.4 M/s** | 2.5 M/s | 2.3 M/s |
| **-c50 -P16 SET** | **4.7 M/s** | 1.9 M/s | 2.0 M/s |
| **-c1 GET** | **86 k/s** | 65 k/s | 48 k/s |
| **-c1 SET** | **72 k/s** | 63 k/s | 54 k/s |

对比 io_uring 的 C 参考实现：kevy 手写绑定达到 148 ns 的 nop 往返，而
liburing 2.9 是 152 ns —— 已贴 Linux 内核底线，且未链接 liburing。可用
[`bench/loopback_c50.sh`](bench/loopback_c50.sh) 和
[`bench/loopback_c1.sh`](bench/loopback_c1.sh) 复现。

### 集群路由（key 感知客户端）

单端口客户端落到错误 shard 时要付一次跨 shard 转发 hop。集群感知的
[`ClusterClient`](#集群模式单机key-感知路由) 把每个 key 直接路由到其属主
shard，消除这一 hop。干净的 lx64 16 核裸机，server/client 异核，GET 并发 64：

| 客户端路径 | 吞吐 | p99 延迟 |
|------------|-----:|---------:|
| 单 shard 代理（跨 shard hop） | 333 k/s | 3858 µs |
| **`ClusterClient`（零 hop）** | **533 k/s** | **260 µs** |

**吞吐 1.6×、尾延迟降约 15×** —— 纯粹来自消除转发 hop，相比手写裸 router
无可测开销。完整方法见 [`docs/cluster.md`](docs/cluster.md)。

### 嵌入式吞吐（进程内，无网络）

把 [`kevy-store`](crates/kevy-store) 放进你的应用直接调用 —— 无 socket、
无 RESP 解析、无 reactor。单核，`Store` API：

| 操作 | 延迟（中位数） | 吞吐 |
|------|-------------:|-----:|
| `get`（命中） | 54 ns | 约 1850 万 ops/s |
| `get`（未命中） | 14 ns | — |
| `set`（覆盖） | 76 ns | 约 1300 万 ops/s |
| `incr` | 86 ns | — |

约为**网络服务器单核吞吐的 3 倍** —— 嵌入式路径省掉了整个线协议层。可用
`cargo run -p kevy-store --example bench_keyspace --release` 复现。

### Pub/sub 扇出（服务器模式）

1 个发布者 → 50 个订阅者，200 000 条消息，16 字节负载。kevy 是 TCP / RESP
路径上最快的 broker：

| 系统 | 交付 msg/s | 相对 valkey |
|------|----------:|----------:|
| Aeron 1.45（IPC，共享内存） | 26.5 M | 3.90× |
| **kevy** | **18.2 M** | **2.68×** |
| ZeroMQ 4.3.5 | 9.3 M | 1.37× |
| redis 7.4 | 8.5 M | 1.25× |
| valkey 9.1 | 6.8 M | 1.00× |
| Zenoh 1.9 | 2.7 M | 0.40× |

Aeron 的共享内存 IPC 是结构性上限（不经内核网络栈）；在 TCP broker 中 kevy
领先 —— 同样的传输下达到 ZeroMQ 的 2 倍。Pub/sub 是**服务器模式**的功能；
嵌入式库是纯键值。方法与 6 路对比工具见
[`bench/pubsub-compare/`](bench/pubsub-compare/)。

### 二进制大小与内存

| | |
|---|---|
| 服务器二进制（`release`，已 strip） | **768 KB** |
| 服务器二进制（`release-min`，`opt-level="s"`） | **640 KB** |
| 空载 RSS（默认 16 线程） | **4.9 MB** |
| 空载 RSS（`--threads 1`） | **2.5 MB** |
| 每 key 内存（800 万 key 时） | 约 190 B（key + value + 表开销） |

`SmallBytes` 把 ≤ 22 B 的负载内联，零堆分配。一个完整的 kevy 服务器是不到
1 MB 的二进制，启动后内存不到 5 MB。

## 快速上手

### 安装

每个 [GitHub Release](https://github.com/goliajp/kevy/releases) 都附带预编译的
`kevy` 服务器二进制。支持的目标：

| 平台 | 归档文件 |
|------|----------|
| Linux x86_64 | `kevy-<TAG>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 | `kevy-<TAG>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `kevy-<TAG>-aarch64-apple-darwin.tar.gz` |

> Windows：kevy 的 OS 层是 POSIX socket + epoll/kqueue + io_uring，没有
> 原生 Windows 构建。请使用下面的 Docker 镜像 —— Windows 上的
> Docker Desktop 会透明地运行 Linux 容器。

一行命令安装（Linux / macOS，按需选择目标）：

```sh
TAG=v1.0.0-rc4
TARGET=x86_64-unknown-linux-gnu      # 或 aarch64-unknown-linux-gnu、aarch64-apple-darwin
curl -L "https://github.com/goliajp/kevy/releases/download/$TAG/kevy-$TAG-$TARGET.tar.gz" | tar -xz
sudo install "kevy-$TAG-$TARGET/kevy" /usr/local/bin/kevy
kevy --port 6004
```

每个归档都包含 `kevy` 二进制、`kevy.toml.example`、`README.md` 以及两份
license。每个资源旁还发布了对应的 `.sha256`。或者按下面从源码构建。

### 使用 Docker 运行

官方镜像在每次发版时同时推送到 Docker Hub
（[`goliakk/kevy`](https://hub.docker.com/r/goliakk/kevy)）和 GitHub
Container Registry
（[`ghcr.io/goliajp/kevy`](https://github.com/goliajp/kevy/pkgs/container/kevy)），
两个 registry 上都是多架构（`linux/amd64` + `linux/arm64`），Tag 相同：
`:<semver>`（如 `:1.0.0-rc6`）、`:rc`（滚动追新 RC）、`:latest`（仅
stable，RC 期不打）。

```sh
# 临时运行
docker run --rm -p 6379:6379 goliakk/kevy:rc

# 持久化（快照 + AOF 通过命名卷在重启后保留）
docker run -d --name kevy -p 6379:6379 -v kevy-data:/data goliakk/kevy:rc
redis-cli -p 6379 SET foo bar
```

镜像默认值：`KEVY_BIND=0.0.0.0`、`KEVY_PORT=6379`、`KEVY_DIR=/data`、
`KEVY_AOF=1`。用 `-e` 覆盖，或在镜像名后面接 CLI 参数：
`docker run ... goliakk/kevy:rc --threads 4 --port 7000`。

Linux 内核 5.13+ 可以启用 io_uring reactor。Docker 默认 seccomp 拦截
`io_uring_setup`，需要放开：

```sh
docker run --rm -p 6379:6379 -e KEVY_IO_URING=1 \
  --security-opt seccomp=unconfined goliakk/kevy:rc
```

更喜欢 GitHub registry？把上面任何 `goliakk/kevy` 替换成
`ghcr.io/goliajp/kevy` 即可 —— 同一镜像、同样 tag。

### 作为服务器

```sh
# 用默认配置构建并运行（仅 loopback，AOF 开启，端口 6004）
cargo run -p kevy --bin kevy --release

# 或使用 TOML 配置文件
cp crates/kevy/kevy.toml.example ./kevy.toml
cargo run -p kevy --bin kevy --release -- --config ./kevy.toml

redis-cli -p 6004 SET foo bar
redis-cli -p 6004 GET foo
```

优先级为 CLI 参数 > 环境变量 > TOML 文件 > 内置默认值：

```sh
kevy --bind 0.0.0.0 --port 7000 --threads 4 --dir /var/lib/kevy
# 等价环境变量：KEVY_BIND  KEVY_PORT  KEVY_THREADS  KEVY_DIR  KEVY_AOF
```

带完整注释的配置 schema 见
[`crates/kevy/kevy.toml.example`](crates/kevy/kevy.toml.example)。

### 集群模式（单机，key 感知路由）

`--cluster`（或 `KEVY_CLUSTER=1` / `[cluster] enabled = true`）把每个 shard
暴露为一个虚拟集群节点：shard `i` 在 `port + 1 + i` 上多开一个确定端口，
`CLUSTER SLOTS / SHARDS / NODES` 上报真实拓扑，错误 shard 的 key 在集群端口上
回 `-MOVED` 而非转发。这不是多机分布式（无 failover / 在线 reshard / gossip），
而是 shard 路由的载体。

```sh
kevy --threads 8 --cluster          # 主端口 6004，shard 端口 6005-6012
redis-cli -c -p 6005 SET foo bar    # 自动跟随 MOVED
```

Rust 调用方可用 [`kevy-client`](crates/kevy-client) 1.9.0 的类型化
`ClusterClient`：一次发现拓扑后，每个 key 按 CRC16 slot 直达属主 shard，
无 `-MOVED`、无转发 hop（即上文 **1.6× 吞吐 / 15× 尾延迟** 的来源）：

```rust
// Cargo.toml: kevy-client = "1.9.0"
use kevy_client::ClusterClient;

let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;  // 任一 shard 端口作种子
cc.set(b"user:42", b"alice")?;                            // 按 CRC16 slot 路由
let v = cc.get(b"user:42")?;
let removed = cc.del(&[b"a", b"b", b"c"])?;               // 多 key 可跨 shard
# Ok::<(), std::io::Error>(())
```

覆盖 string / hash / list / set / zset / del / exists / dbsize / flushall /
ping / publish；完整指南、命令表与 same-slot 规则见
[`docs/cluster.md`](docs/cluster.md)。

### 资源自适应设计

kevy 对资源遵循一条原则：**有空间就释放性能，没空间就保命，到边界就硬卡，
且永远大声报错——绝不静默。**

- **无上限 = 满速**：`maxmemory = 0`（默认）时零记账开销（eviction 记账被编译期
  跳过，单条不命中分支）。没设的限制不收你一分钱。
- **有上限 = 优雅淘汰**：设 `maxmemory` + 策略（LRU / LFU / Random / TTL，共 8
  种）后,写入会采样淘汰 key 直到回落到限额 **5% 以下**（留 headroom，下一次写
  不会立即再触发淘汰）。
- **边界 = 大声拒绝，不损坏**：`NoEviction`（默认策略）下，会超预算的写在执行前
  就以 Redis 经典 `OOM` 错误被拒（热路径 O(1) precheck）。只卡**会增长内存**的
  动词；收缩类（`DEL`/`LPOP`/`SREM`/`EXPIRE`…）和 `FLUSH*` 永远放行，让你总能
  救回实例。
- **能力降级，不崩溃**：io_uring 启动时探测，旧内核 / seccomp 沙箱下**回退到
  epoll**；`wasm32` 嵌入构建以宿主喂时钟 + 受限 surface 运行而非拒绝构建；
  非 loopback 的 `--bind` 会**打印警告**（kevy 无 AUTH/TLS）而非静默暴露。

### 作为嵌入式库

```rust
// Cargo.toml: kevy-store = "0.1"
use kevy_store::Store;

let mut s = Store::default();
s.set(b"key".to_vec(), b"value".to_vec(), None, false, false);
assert_eq!(s.get(b"key").unwrap().unwrap(), b"value");
```

## 何时使用 kevy

kevy v1.0 已经为以下四种场景做好了生产就绪：

1. **本地开发** —— `cargo run -p kevy` 配上你惯用的 Redis 客户端。
2. **docker-compose 内部** —— 网络内设 `KEVY_BIND=0.0.0.0`；信任边界就是
   docker 网络本身。
3. **嵌入式库** —— 把 [`kevy-store`](crates/kevy-store) 直接放进你的应用：
   无网络、无 reactor。
4. **缓存** —— 前面挡着一个真正的数据库，kevy 用 TTL + `maxmemory` +
   LRU / LFU 淘汰来托管热数据。

**设计上不在范围内：** 复制、集群、AUTH / TLS，以及直接暴露到公网。
若需要高可用 / 多机，请用 Kubernetes StatefulSet 或 sidecar 代理模式。
完整的范围取舍说明与 94 条命令对等表见
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md)。

## Crates

kevy 由一组小而可复用的 crate 构成 —— 8 个可发布的库，外加服务端内部组件：

| crate | 职责 |
|-------|------|
| [`kevy-bytes`](crates/kevy-bytes) | 自有字节串，内联或堆分配的小字符串优化 |
| [`kevy-hash`](crates/kevy-hash) | 面向单一信任域 keyspace 的快速非加密 hash |
| [`kevy-map`](crates/kevy-map) | 带 SIMD 分组扫描的 Swiss-table hashmap |
| [`kevy-resp`](crates/kevy-resp) | 零分配 RESP2 / 3 解析器 |
| [`kevy-ring`](crates/kevy-ring) | 有界无锁 SPSC 队列 |
| [`kevy-madvise`](crates/kevy-madvise) | Linux `MADV_HUGEPAGE` 封装，其他平台为 no-op |
| [`kevy-uring`](crates/kevy-uring) | 纯 Rust io_uring 绑定，不依赖 liburing |
| [`kevy-resp-client`](crates/kevy-resp-client) | 阻塞式 RESP2 客户端 |
| `kevy-config` · `kevy-store` · `kevy-rt` · `kevy-persist` | 配置、keyspace、运行时、持久化 |
| `kevy-sys` | 唯一的 libc 边界（服务端内部） |
| `kevy` | 服务器二进制 |

## 命令

五种 Redis 数据类型 —— **String、Hash、List、Set、Sorted Set** —— 外加
**Streams**（`XADD` / `XREAD` / `XRANGE` / 消费者组）、**阻塞弹出**
（`BLPOP` / `BRPOP` / `XREAD BLOCK` / `XREADGROUP BLOCK` —— 单键与多键、
**可跨分片**）、**pub/sub**（`SUBSCRIBE` / `PSUBSCRIBE` —— 模式 glob）、
**事务**（`MULTI` / `EXEC` / `DISCARD` / `WATCH` / `UNWATCH` —— 乐观 CAS）、
持久化（`SAVE` / `BGSAVE` / `BGREWRITEAOF`）和运维命令（`INFO` / `CONFIG`
（真正的热修改）/ `CLIENT` / …）。多键命令、pub/sub、WATCH 和阻塞弹出都能
跨每核分片工作，`WRONGTYPE` 的行为与 Redis 一致。

带 valkey 对等说明的完整命令列表见
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md)。

## 构建与测试

```sh
cargo build --workspace --release
cargo test  --workspace
bash bench/run.sh        # 与 valkey 对打的基准（Linux + Docker）
```

稳定版 Rust 1.95，Rust 2024 edition。可在 Linux（`x86_64`、`aarch64`）和
macOS 上构建。`kevy-embedded` 及其依赖闭包还能为
`wasm32-unknown-unknown` / `wasm32-wasip1` 构建 —— WebAssembly 演示见
[`docs/wasm.md`](docs/wasm.md)。

## 路线图与稳定性

kevy 正处于 **v1.0.0-rc** 反馈期。v1.x 承诺保持不变的一切 —— 持久化格式、
RESP 线协议、公开 Rust API、CLI 参数、环境变量、TOML schema、淘汰语义 ——
在整个 v1.x 线上都是**只增不改**：v1.0 写出的文件能在任何后续 v1.x 构建上
加载。完整的稳定性契约见
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md#v1x-stability-commitment)。

## 许可证

按你的选择，采用 **MIT** 或 **Apache-2.0** 双许可之一。
© 2026 GOLIA K.K.
