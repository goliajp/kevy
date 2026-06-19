# kevy

[English](README.md) · **简体中文** · [日本語](README.ja.md)

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
![Rust stable](https://img.shields.io/badge/rust-stable-orange.svg)

纯 Rust、**零依赖**、Redis 协议兼容的键值存储 —— 既可作独立服务器,也
可作嵌入式库,设计目标是榨干硬件极限。

kevy 说 Redis 线协议(RESP2),`redis-cli` / `valkey-cli` / 任何 Redis
客户端库都能**无改动**接入。底层是 thread-per-core / shared-nothing
现代架构,完全 Rust 实现 —— 唯一接触 C 的地方是无法绕过的 OS 系统调用边界。

```sh
cargo run -p kevy --bin kevy --release      # 回环,AOF 开,端口 6004
redis-cli -p 6004 SET hello world
```

## 为什么选 kevy

- **快** —— 高并发下吞吐 2.7-3.0× valkey 9.1,pub/sub 扇出 2.7×,嵌入式
  每核 **~9 M GET / 7 M SET**(数字见下文)。
- **占用极小** —— 768 KB 服务器二进制,启动后驻留 5 MB 以内的 RAM。
  适合容器 sidecar、小 VM、边缘盒子。
- **现代架构** —— thread-per-core、shared-nothing、热路径无锁、Linux 上
  io_uring。没有全局锁、没有 GIL 类似的瓶颈。
- **零供应链风险** —— 默认的服务器 / 阻塞客户端 / 嵌入式栈在 crates.io 上
  零依赖。整棵代码树是 `std` + kevy 自家 crate;唯一的 C 是 OS 系统调用
  边界,手写绑定在一个 crate 里。异步客户端(`kevy-client-async`)是唯一
  开口的例外 —— opt-in、仅 lib 消费者使用、文档透明记录。
- **协议兼容** —— RESP2 线协议,98 个命令与 valkey 9.1 对齐(含模式
  pub/sub 与 `WATCH`/`UNWATCH` 乐观 CAS),逐字节比对验证回复。现有客户
  端和工具直接可用。
- **可复制集群**(v1.22)—— 服务器主从 + N 只读副本 + 仲裁失败切换,
  **嵌入式节点可作为只读副本或按前缀的 writer 加入集群**。同一套线协议
  贯穿,单份声明式拓扑。
- **可嵌入** —— `kevy-store` 是纯 Rust 库:无网络、无 runtime,也能构建
  到 `wasm32`。同一引擎,跑在你进程里。
- **异步可用** —— `kevy-client-async`(v1.22)1:1 镜像阻塞客户端表面,
  支持 `tokio` / `smol` / `async-std`,并提供 pipeline-first builder
  把 N 条命令折叠成单次 TCP 往返。
- **资源自适应** —— 内存不受限时全速运行,有限时优雅退化,边界处响亮
  地拒绝而不是默默腐化数据([详见](#资源自适应设计))。

诚实陈述范围:kevy 是**单 DC** 设计,没有 AUTH/TLS,也没有面向公网的部
署设计(见[何时使用 kevy](#何时使用-kevy))。复制是单 DC 主从 + 仲裁切
换;跨 DC active-active、gossip、在线 resharding、Raft 都明确不在范围内。

## 性能

下面所有数字都在一台 **16 核裸金属 Linux** 机器(lx64)上测得,纯内存,
服务器 / 客户端 / 负载机分别 pin 到不相交的 CPU。所有 bench 可用
[`bench/`](bench/) 里的脚本复现;完整方法、注意事项、v0.2 → v1.22 的时
间线叙事都在 [`bench/REPORT.md`](bench/REPORT.md)。

### 服务器吞吐(走网络)

> 跑赢 valkey 9.1 是底线,不是目标 —— kevy 瞄准的是硬件天花板。

`redis-benchmark`,服务器 pin 在 0-9 核、客户端在隔离核上,**单独**运行
每个引擎(启动 → 2 个热身 run → 关停),让 kevy 的 busy-poll 不会饿死同
驻的竞争者。每个引擎都用最快配置(valkey/redis 都开 `--io-threads 10`):

| 工作负载 | kevy 1.22 | valkey 9.1 (io-threads) | redis 7.4 (io-threads) |
|----------|----------:|------------------------:|-----------------------:|
| **-c50 -P16 GET** | **6.0 M/s** | 2.0 M/s | 2.0 M/s |
| **-c50 -P16 SET** | **4.0 M/s** | 1.5 M/s | 1.5 M/s |
| **-c1 GET** | **68 k/s** | 60 k/s | 55 k/s |
| **-c1 SET** | **76 k/s** | 60 k/s | 54 k/s |

→ kevy 在高并发下 **GET 3.0× / SET 2.7× 优于次优者**,单连接 sequential
(任何 busy-poll 引擎最难的工作负载)上也领先 1.13-1.26×。io_uring vs
epoll 看负载形态(io_uring 在低并发领先,epoll 在 -c50 -P16 因 pipelining
摊薄系统调用开销而追上)。用 [`bench/loopback_c50.sh`](bench/loopback_c50.sh)
和 [`bench/loopback_c1.sh`](bench/loopback_c1.sh) 复现。

对 io_uring 的 C 参考:kevy 手写绑定 148 ns 完成空 round-trip,vs
liburing 2.9 的 152 ns —— 已到 Linux 内核地板,且没链 liburing。

### 集群路由(key 感知客户端)

单端口客户端落到错误 shard 时会付出一次内部跨 shard 转发开销。集群感知
的 [`ClusterClient`](#集群模式单机key-感知路由) 把每个 key 路由到拥有
它的 shard,完全去掉那次跳转。lx64 16 核,服务器/客户端不相交,GET 并发
64:

| 客户端路径 | 吞吐 | p99 延迟 |
|----------|----:|-------:|
| 单 shard 代理(跨 shard 跳) | 333 k/s | 3858 µs |
| **`ClusterClient`(零跳转)** | **533 k/s** | **260 µs** |

**吞吐 1.6×、尾延迟约 15× 下降** —— 完全来自去掉转发跳,跟手写裸路由相
比无可测开销。完整方法在 [`docs/cluster.md`](docs/cluster.md)。

### 集群模式(复制 + 失败切换 + 嵌入加入)

v1.22 收尾 v3-cluster 线。一个 kevy 节点可以作为 **primary** 把每次
mutation 串流推给 N 个 replica,或作为 **replica** 镜像 primary;**嵌入
式节点能加入集群**,作只读副本或按前缀的 writer;`kevy-elect` 在 primary
DOWN 时执行**仲裁自动切换**。配套客户端 `kevy-cluster-rw` 把写发往
primary、读 round-robin 跨 replica。

```toml
# primary
[replication]
role = "primary"
listen_port_base = 16004

# replica
[replication]
role = "replica"
upstream = "primary.example:16004"
```

```sh
# 运行时通过 Redis 兼容命令重定位 / 提升。
redis-cli -p 6004 REPLICAOF primary.example 16004
redis-cli -p 6004 REPLICAOF NO ONE
redis-cli -p 6004 ROLE
```

各阶段功能(在 v1.22 已全部合入):
- **Phase 1**(v1.18):per-shard 线 backlog + listener、追赶 replica 的
  快照 ship、动态 REPLICAOF / `REPLICAOF NO ONE` 重定向 + 降级、
  `ROLE` / `INFO replication` 实时状态、`kevy-cluster-rw` 读写分裂客户端。
- **Phase 1.5**(v1.19):`kevy-elect` 仲裁自动 primary 切换(心跳 DOWN
  检测、OFFER/ACCEPT/ANNOUNCE、最高 offset 当选)。
- **Phase 2**(v1.22):**嵌入式节点可作只读副本加入集群** —— 应用嵌入
  `kevy-embedded` 后订阅 primary 的复制流,进程内镜像 keyspace。读零网
  络 round-trip;本地写返回 `READONLY`。
- **Phase 3**(v1.22):**按 scope 多 writer** —— `[cluster] scopes =
  "app:billing:=embed-a,app:catalog:=embed-b"` 声明按前缀的 writer 所有
  权;落到错前缀的节点回复 `-MISDIRECTED writer is <host:port>`。运维触发
  的 `MOVE-SCOPE` 在 quiesce-window 协议下迁移一个前缀。

反范围(永久不做):多 master 重叠、跨 DC active-active / CRDT、Raft、
gossip 发现、在线 resharding、AUTH/TLS。

完整服务器 + 客户端配方在 [`docs/replication.md`](docs/replication.md)
和 [`docs/cluster.md`](docs/cluster.md)。

### 嵌入式吞吐(进程内,无网络)

把 [`kevy-embedded`](crates/kevy-embedded) drop 进你的应用,直接调用
`Store` —— 无 socket、无 RESP 解析、无 reactor。lx64 进程内 bench(1 M
ops,12 字节 key,16 字节 value):

| 操作 | 延迟 | 吞吐 |
|------|-----:|----:|
| `get`(命中) | 111 ns | **9.0 M ops/s** |
| `get`(未命中) | 24 ns | **42.2 M ops/s** |
| `set`(覆盖) | 143 ns | **7.0 M ops/s** |
| `incr` | 169 ns | 5.9 M ops/s |
| `del` | 183 ns | 5.5 M ops/s |

大约是同主机网络服务器 GET 的 **130×、SET 的 90×** —— 嵌入式路径跳过
整个 wire 层(RESP 编解码 + TCP + 系统调用)。用
`cargo run -p kevy-embedded --example embed_throughput --release` 复现。

> 这不是 kevy-vs-valkey/redis 的吞吐声明 —— valkey 和 redis 没有进程内
> 模式,所以唯一公平的说法是"嵌入式跳过 wire 层,省了这么多"。

### Pub/sub 扇出(服务器模式)

1 个发布者 → 50 个订阅者,200 000 条消息,16 字节负载,热身后跑。在 TCP /
RESP 路径上 kevy 是最快的 broker:

| 系统 | 投递 msg/s | vs valkey |
|------|----------:|---------:|
| Aeron 1.45(IPC、共享内存) | 84 M | 12.4× |
| **kevy 1.22** | **18.5 M** | **2.72×** |
| ZeroMQ 4.3.5 | 9.4 M | 1.38× |
| redis 7.4 | 8.9 M | 1.31× |
| valkey 9.1 | 6.8 M | 1.00× |
| Zenoh 1.9 | 2.9 M | 0.43× |

Aeron 的共享内存 IPC 是结构性天花板(没有内核网络栈);在 TCP broker 里
kevy 领先 —— 2× ZeroMQ 同传输,还压过非 broker 的 ZeroMQ direct
messaging。Pub/sub 是**服务器模式**特性;嵌入式库是纯键值。方法 +
6-way harness:[`bench/pubsub-compare/`](bench/pubsub-compare/)。

### 二进制大小与内存

| | |
|---|---|
| 服务器二进制(`release`、stripped) | **768 KB** |
| 服务器二进制(`release-min`、`opt-level="s"`) | **640 KB** |
| 空载 RSS(默认 16 线程) | **4.9 MB** |
| 空载 RSS(`--threads 1`) | **2.5 MB** |
| 每 key 内存(8.6 M keys 时) | ~190 B(key + value + 表开销) |

`SmallBytes` 把 ≤ 22 B 的 payload 内联,零堆分配。完整 kevy 服务器是
亚 MB 二进制,启动后驻留 5 MB RAM 以内。

## 快速上手

### 安装

预编译的 `kevy` 服务器二进制挂在每个 [GitHub Release](https://github.com/goliajp/kevy/releases)
上。支持平台:

| 平台 | archive |
|------|---------|
| Linux x86_64 (glibc) | `kevy-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| Linux aarch64 (glibc) | `kevy-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` |
| macOS aarch64 (Apple Silicon) | `kevy-vX.Y.Z-aarch64-apple-darwin.tar.gz` |

或者从源码编译:

```sh
git clone https://github.com/goliajp/kevy
cd kevy
cargo build -p kevy --bin kevy --release
./target/release/kevy --port 6004
```

### 用 Docker 运行

```sh
# 主线镜像:基于 distroless 的 kevy server。
docker run --rm -p 6004:6004 ghcr.io/goliajp/kevy:1.22 \
  kevy --bind 0.0.0.0 --port 6004
```

镜像包含 `kevy` 和 `kevy-cli`(redis-cli 替代品),并设了 HEALTHCHECK
监控 RESP `PING` 回复。

### 作为服务器

```sh
# 默认:回环、AOF 关、端口 6004。
cargo run -p kevy --bin kevy --release
```

配置文件(可选):

```toml
# kevy.toml
port = 6004
bind = "127.0.0.1"
threads = 8           # shard 数,默认 = CPU 数
persist_dir = "/var/lib/kevy"
aof = true
```

通过 `kevy --config kevy.toml` 装载,或者完全用环境变量:`KEVY_BIND`、
`KEVY_PORT`、`KEVY_THREADS`、`KEVY_AOF`、`KEVY_IO_URING`。

### 集群模式(单机,key 感知路由)

```sh
kevy --threads 8 --cluster          # 主端口 6004、shard 端口 6005-6012
redis-cli -c -p 6005 SET foo bar    # 自动跟 MOVED
```

对 Rust 调用者,[`kevy-client`](crates/kevy-client) 1.11 提供了类型化
`ClusterClient` —— 一次发现拓扑,然后把每个 key 路由到拥有它的 shard,
无 `-MOVED` 无转发跳(就是上面那个 **1.6× 吞吐 / 15× 尾延迟** 胜利):

```rust
// Cargo.toml: kevy-client = "1.11"
use kevy_client::ClusterClient;

let mut cc = ClusterClient::connect("127.0.0.1", 6005)?;  // 任一 shard 端口作种子
cc.set(b"user:42", b"alice")?;                            // 按 CRC16 slot 路由
let v = cc.get(b"user:42")?;
let removed = cc.del(&[b"a", b"b", b"c"])?;               // 多 key 可跨 shard
# Ok::<(), std::io::Error>(())
```

它包了 string / hash / list / set / sorted-set / del / exists /
dbsize / flushall / ping / publish;完整指南、命令表、same-slot 规则在
[`docs/cluster.md`](docs/cluster.md)。当一个客户端推的负载大到 hop 显
眼时用它;普通情况下单端口 `Connection` 仍然正确且更简单。

与 Redis Cluster 的超集说明(单机集群模式 —— 无 gossip / MIGRATE-ASK /
在线 resharding):跨 slot 多 key 命令(`MGET`、`SUNION`、事务、阻塞扇
出)直接执行而不是 `-CROSSSLOT` 失败;keyspace 级视图(`KEYS`、`SCAN`、
`DBSIZE`)在每个端口都保持全 keyspace 视图。已有数据目录在集群模式间切
换会启动时一次性 re-home(原文件备份为 `*.premigration.<ts>`)。

要带 primary + replicas + 自动失败切换的多节点集群,见上方**集群模式
(复制 + 失败切换 + 嵌入加入)**段 —— v1.22 已交付 server-as-replica、
embed-as-replica、按 scope 多 writer、仲裁提升。

### 作为异步运行时客户端

已经跑在 `tokio` / `smol` / `async-std` 上的应用,用阻塞客户端的异步镜像:

```rust
// Cargo.toml: kevy-client-async = { version = "1", features = ["tokio"] }
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;

// 把 N 条命令 pipeline 成单次 TCP round-trip:
let replies = conn.pipeline()
    .set(b"a", b"1").get(b"a").incr(b"hits")
    .run(&mut conn).await?;
# Ok::<(), std::io::Error>(())
```

必须显式选一个 runtime feature(`tokio` / `smol` / `async-std`);零个或
两个以上编译报错。阻塞的 [`kevy-client`](crates/kevy-client) 仍是默认且
保持 0 依赖 —— async 是 opt-in。完整指南 + runtime 比较 + 何时 pipeline:
[`docs/async.md`](docs/async.md)。

### 作为嵌入式库

```rust
// Cargo.toml: kevy-embedded = "1.4"
use kevy_embedded::{Config, Store};

let s = Store::open(Config::default().without_aof())?;
s.set(b"key", b"value")?;
assert_eq!(s.get(b"key")?, Some(b"value".to_vec()));
# Ok::<(), std::io::Error>(())
```

`Store` 处处 `&self` —— 在线程间随便 clone,shard 内部自己锁。需要持久
化文件存储用 `Config::default().with_persist("/var/lib/myapp")`。要让
嵌入式作为服务器 primary 的只读副本(v1.22),见
[`docs/replication.md`](docs/replication.md)。

## 资源自适应设计

kevy 关于资源遵守一条规则:**有空间就跑全速,没空间就活下去,边界处硬
拒绝,大声失败 —— 永不静默**。这贯穿引擎:

- **无界 = 全速**。`maxmemory = 0`(默认)时,会计开销编译期就被单分
  支判定优化掉。你没设置的限制就完全不付任何代价。
- **有界 = 优雅 eviction**。设 `maxmemory` + 策略(LRU / LFU / Random /
  TTL,共 8 种),写命令把采样的 key 驱逐到**限额下 5%** —— 留头部空间
  让下一次写不会立即又进入 eviction。
- **边界 = 大声拒绝,不腐化**。`NoEviction`(默认策略)下,会超预算的
  写在执行前就以 Redis 经典 `OOM` 错误被拒绝 —— 热路径上 O(1) 预检。只
  对内存**增长**的动词加门,缩减(`DEL` / `LPOP` / `SREM` / `EXPIRE` /
  …)和 `FLUSH*` 总能过,所以满实例总能恢复。
- **能力降级,不崩**。io_uring 启动时探测,**回退到 epoll** 在旧内核 /
  seccomp 沙箱里(可用 `KEVY_IO_URING` 强制)。`wasm32` 嵌入式构建走
  host 喂时钟、surface 缩水,而不是构建失败。非回环 `--bind` **打 warning**
  (kevy 无 AUTH/TLS),而不是默默暴露你。

集群感知的 [`ClusterClient`](#集群模式单机key-感知路由) 在客户端遵循同
样哲学:当负载让 hop 显眼时花连接数去跳过它,平时停在简单单端口上。

## 何时使用 kevy

✅ 适合:
- 内部缓存 / 会话存储 / 速率限制 / 排行榜 / 计数 / pub/sub 总线
- 边缘盒子 / VM sidecar / 容器内同主机协作进程
- Rust 应用需要进程内 KV(以及可选的本地集群 join)
- 在更大的 redis 兼容 KV 之前的快速、可信反检索基线

❌ 不适合:
- 公网或多租户 SaaS 部署(无 AUTH/TLS,永久不会有)
- 跨 DC 主主复制 / 强一致性需求(单 DC primary-replica + 仲裁失败切
  换 —— 是这个范围)
- 持久数据库 ACID / 全文搜索 / 时序 / 关系查询(不在范围内 —— 用专门
  的存储)

## Crates

主要的 publish 到 crates.io 的 crate:

| crate | 用途 |
|-------|------|
| [`kevy`](crates/kevy) | 服务器二进制 `kevy` 与 `kevy-cli` |
| [`kevy-embedded`](crates/kevy-embedded) | 嵌入式 `Store` + Config + replica/writer 加入 |
| [`kevy-client`](crates/kevy-client) | 阻塞客户端 + `ClusterClient` |
| [`kevy-client-async`](crates/kevy-client-async) | 异步客户端(tokio/smol/async-std) |
| [`kevy-store`](crates/kevy-store) | 底层 shard `Store`(嵌入用,无 config 装配) |
| [`kevy-resp`](crates/kevy-resp) | RESP2/3 编解码 |
| [`kevy-resp-client`](crates/kevy-resp-client) | 阻塞 RESP 客户端基础 |
| [`kevy-scope`](crates/kevy-scope) | 按 scope 的 writer 所有权(P3) |
| [`kevy-replicate`](crates/kevy-replicate) | 复制流协议 + 客户端 |
| [`kevy-elect`](crates/kevy-elect) | 仲裁切换协议 |
| [`kevy-cluster-rw`](crates/kevy-cluster-rw) | 读写分裂 + scope 路由客户端 |

其他次要 crate(`kevy-bytes` / `kevy-hash` / `kevy-map` / `kevy-rt` /
`kevy-persist` / `kevy-sys` / `kevy-uring` / `kevy-madvise` / `kevy-ring` /
`kevy-config` / `kevy-geo`)也都在 crates.io,组合时可单独取用。

## 嵌入式 ↔ 服务器,一个 URL

[`kevy-client`](crates/kevy-client) 把两个后端藏在同一个 URL 接口下,所
以业务代码可以**用 URL 字符串切换** in-process 嵌入式 / TCP 服务器:

| URL | 后端 |
|-----|------|
| `mem://` | 进程内嵌入式,纯内存,匿名 bus |
| `mem://<name>` | 进程内嵌入式,纯内存,**命名 shared bus**(同 name 不同 open 看到一致 pub/sub 总线) |
| `file:///abs/path` | 进程内嵌入式 + 持久化(AOF) |
| `kevy://host[:port][/db]` | TCP RESP,kevy 原生别名 |
| `redis://host[:port][/db]` | TCP RESP,标准 Redis URL |
| `tcp://host[:port]` | TCP RESP,纯地址(无 SELECT 头) |

```rust
use kevy_client::Connection;

let url = std::env::var("MY_KEVY_URL").unwrap();
let mut c = Connection::open(&url)?;
c.set(b"hello", b"world")?;
assert_eq!(c.get(b"hello")?, Some(b"world".to_vec()));
# Ok::<(), std::io::Error>(())
```

dev/test 用 `mem://`、staging 用 `file:///tmp/staging`、prod 用
`kevy://prod-host:6004` —— 业务代码不变。

## 命令

参见 [`docs/COMMANDS.md`](docs/COMMANDS.md)。简短的话:**string / hash /
list / set / sorted-set / 模式 pub/sub** 完整;**transactions**
(`MULTI`/`EXEC`/`WATCH`)完整;**streams**(`XADD`/`XREAD`/`XLEN`)子集;
**keyspace 通知**(`__keyspace@*__:*` / `__keyevent@*__:*`)完整。

`SCAN` / `HSCAN` / `SSCAN` / `ZSCAN` 完整 cursor 实现。`OBJECT
ENCODING` 在每个类型上回退到 valkey 期望的字符串(`ziplist` /
`hashtable` / `intset` / …)以兼容那些数据形态嗅探的工具。

不支持:`CLUSTER`(子集 —— 见 [`docs/cluster.md`](docs/cluster.md))、
`SCRIPT EVAL`(等 luna runtime 就绪)、`MODULE`、`MIGRATE` / `ASK`、
`AUTH` / `ACL`。

## 构建与测试

```sh
# 整 workspace 编译 + 单元测试。
cargo build --workspace --release
cargo test --workspace --release

# Bench(本地,需要 lx64 类机器才有公平数字)。
bash bench/loopback_c50.sh
bash bench/loopback_c1.sh
bash bench/pubsub-compare/run.sh
```

CI 跑 stable Rust(无 MSRV pin)+ `-D warnings` clippy + miri(advisory
FFI 在 miri 下 short-circuit)+ Docker 镜像构建 + 多目标 cross-build。

## 路线图与稳定性

- **v1.22**(2026-06-20,已发布)—— v3-cluster bundle:嵌入式只读副本 +
  按 scope 多 writer + 异步客户端。详见 [`CHANGELOG.md`](CHANGELOG.md)。
- **v1.22.x follow-up**(无固定时间)—— 多 shard upstream 副本、滚到尾
  巴的 backlog 偏移快照 ingest、F4 fallback 路径的 writer 自动 reclaim。
- **v2-8 Lua**(等 luna runtime 就绪)—— `SCRIPT EVAL` / `EVALSHA` 通过
  自家 Lua 5.5 runtime 实现,作为 kevy 的 plugin,不在 server 内塞解释器。

API 稳定性:已 publish 的 crate 跟 semver(主版本 1.x);默认 server 保
持向后兼容 wire 协议;嵌入式 API 加 surface 走 minor 版本号。

## 许可证

MIT OR Apache-2.0,选你顺手的。
