# kevy

[English](README.md) · **简体中文** · [日本語](README.ja.md)

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![Rust stable](https://img.shields.io/badge/rust-stable-orange.svg)

一个纯 Rust、零依赖、兼容 Redis 的键值存储。可以作为独立服务器使用、
作为进程内库使用,或两者并用 —— 每种形态都讲 RESP2,所以 `redis-cli`
和所有 Redis 客户端库无需任何修改即可工作。

```sh
cargo install kevy
kevy --port 6379 &
redis-cli -p 6379 SET hello world
redis-cli -p 6379 GET hello
```

## kevy 是什么

kevy 以三种形态提供,全部由同一个引擎构建:

- **服务器** — 一个兼容 Redis 协议的守护进程。讲 RESP2,98 条命令
  的回复逐字节对照 valkey 9.1 校验。
- **嵌入式库** — `kevy-embedded` 是同一个引擎,只是去掉了网络层。
  把它放进一个 Rust 二进制里,直接调用 `Store`。纯 Rust、零依赖,
  可以构建到 `wasm32`。
- **客户端** — `kevy-client`(阻塞式)与 `kevy-client-async`(每种
  运行时一个 feature flag:tokio / smol / async-std)。两者都接受
  一个 URL,所以同一段代码既能对接 TCP 服务器(`kevy://host:port`),
  也能对接进程内总线(`mem://name`)。

## 我应该选哪一个

| 场景 | 选择 |
|---|---|
| 我已有 Redis 客户端库,想要一个更快、更轻的 Redis | 服务器(`kevy`) |
| 我有一个 Rust 应用,不想再跑一个单独的进程 | 嵌入式库(`kevy-embedded`) |
| 我写 Rust,想跟 kevy 或 Redis 服务器通信 | `kevy-client`(阻塞式) |
| 我写 Rust,基于 `tokio` / `smol` / `async-std` | `kevy-client-async` |
| 我想让同一份代码用一个 URL 在嵌入式和服务器之间切换 | `kevy-client` + `kevy-embedded` |

## 安装

```sh
# 服务器
cargo install kevy

# 嵌入式库
cargo add kevy-embedded

# 阻塞客户端
cargo add kevy-client

# 异步客户端(选一个运行时 feature)
cargo add kevy-client-async --features tokio
```

预构建的服务器二进制随每个 [GitHub Release](https://github.com/goliajp/kevy/releases)
附带,覆盖 Linux x86_64、Linux aarch64 和 macOS Apple Silicon。
多架构 Docker 镜像同时发布到 [Docker Hub](https://hub.docker.com/r/goliakk/kevy)
和 [GitHub Container Registry](https://github.com/goliajp/kevy/pkgs/container/kevy):

```sh
docker run --rm -p 6379:6379 goliakk/kevy:latest
```

## 快速开始

### 服务器

```sh
kevy --port 6379 &
redis-cli -p 6379 SET foo bar
redis-cli -p 6379 GET foo
```

配置优先级从高到低为:CLI 参数 → 环境变量 → TOML 文件 → 内置默认值。
完整的带注释 schema 见 [`crates/kevy/kevy.toml.example`](crates/kevy/kevy.toml.example)。

### 嵌入式库

```rust
use kevy_embedded::{Config, Store};

let store = Store::open(Config::default().without_aof())?;
store.set(b"key", b"value")?;
assert_eq!(store.get(b"key")?, Some(b"value".to_vec()));
# Ok::<(), std::io::Error>(())
```

`Store` 实现了 `Clone`,所有方法都接收 `&self`,所以克隆可以在线程
之间自由移动。要用文件支持的存储,使用
`Config::default().with_persist("/var/lib/myapp")`。

### 阻塞客户端

```rust
use kevy_client::Connection;

let mut conn = Connection::open("tcp://127.0.0.1:6379")?;
conn.set(b"k", b"v")?;
let v = conn.get(b"k")?;
assert_eq!(v.as_deref(), Some(&b"v"[..]));
# Ok::<(), std::io::Error>(())
```

同一套 URL 表面也接受 `mem://app` 作为进程内后端,所以同一套代码
路径在测试中跑嵌入式存储、在生产中跑联网服务器。

### 异步客户端

```rust,no_run
use kevy_client_async::AsyncConnection;

# async fn run() -> std::io::Result<()> {
let mut conn = AsyncConnection::open("tcp://127.0.0.1:6379").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
# Ok(())
# }
```

从 `tokio`、`smol`、`async-std` 中**恰好**选一个作为 Cargo feature;
零个或多个 feature 都会让 crate 拒绝编译。

## 性能

来自裸机基准测试套件的一段代表性切片(16 核 Linux 机器,服务器和
客户端分别 pin 在不相交的核心上,TCP loopback,精确模式 CI95 < 1%)。
完整方法、每种 workload 以及注意事项见 [`bench/REPORT.md`](bench/REPORT.md);
每个数字都可以由 [`bench/`](bench/) 里的脚本复现。

| Workload | kevy | valkey 9.1 | 比值 |
|---|---:|---:|---:|
| `SET -c 1` | 94.7 k/s | 62.2 k/s | **1.52×** |
| `GET -c 1` | 97.3 k/s | 65.0 k/s | **1.50×** |
| `SET -c 50 -P 16` | 2.59 M/s | 1.82 M/s | **1.42×** |
| Pub/sub 扇出(50 个订阅者) | 23.1 M/s | 5.1 M/s | **4.52×** |
| 嵌入式 `get`(命中) | 9.0 M/s | — | (无进程内 Redis) |
| 嵌入式 `set`(覆写) | 7.0 M/s | — | (无进程内 Redis) |

一个完整的服务器是一个 768 KB 的 stripped 二进制,启动后驻留内存
不到 5 MB。

## 兼容性

98 条命令的回复对照 valkey 9.1 逐字节校验,覆盖全部五种 Redis 数据
类型(String、Hash、List、Set、Sorted Set)外加 Streams、Pub/Sub
(频道 + 模式)、事务(`MULTI` / `EXEC` / `WATCH` / `UNWATCH`)、
阻塞式 pop,以及标准的运维操作和持久化动词。完整命令清单见
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md)。

针对 kevy 端到端验证过的客户端库:

| 语言 | 库 | 版本 |
|---|---|---|
| Java | [Jedis](https://github.com/redis/jedis) | 5.x |
| .NET | [StackExchange.Redis](https://stackexchange.github.io/StackExchange.Redis/) | 2.x |
| Go | [go-redis](https://github.com/redis/go-redis) | v9 |
| Python | [redis-py](https://github.com/redis/redis-py) | 5.x |
| Python | [Celery](https://docs.celeryq.dev/) | 5.6 |
| Ruby | [Sidekiq](https://sidekiq.org/) | 6.5 |
| Node.js | [ioredis](https://github.com/redis/ioredis) | 5.7 |
| Node.js | [BullMQ](https://github.com/taskforcesh/bullmq) | 5.79 |
| Node.js | [Bee Queue](https://github.com/bee-queue/bee-queue) | 1.7 |
| Node.js | [node-redlock](https://github.com/mike-marcacci/node-redlock) | 5 |

全部针对一个默认的 `kevy --port 6379` 实例不加修改地运行通过。

## Crate 一览

| Crate | 角色 |
|---|---|
| [`kevy`](crates/kevy) | 服务器二进制和库入口 |
| [`kevy-embedded`](crates/kevy-embedded) | 进程内 KV,提供 Redis 形态的 Rust API |
| [`kevy-client`](crates/kevy-client) | 阻塞式 RESP 客户端;URL 立面统一服务器和进程内后端 |
| [`kevy-client-async`](crates/kevy-client-async) | `kevy-client` 的异步镜像,支持 tokio / smol / async-std |
| [`kevy-cluster-rw`](crates/kevy-cluster-rw) | 主写 / 从读的客户端封装 |
| [`kevy-cli`](crates/kevy-cli) | 运维 CLI:备份、恢复、冒烟测试 |
| [`kevy-config`](crates/kevy-config) | TOML 配置 schema,处理 CLI/env/file 的优先级 |
| [`kevy-resp-client`](crates/kevy-resp-client) | 底层 RESP2 客户端原语 |
| [`kevy-bytes`](crates/kevy-bytes) | 拥有所有权的字节串,带 inline-or-heap 短字符串优化 |
| [`kevy-hash`](crates/kevy-hash) | 面向单信任域 keyspace 的快速非加密哈希 |
| [`kevy-map`](crates/kevy-map) | Swiss-table 哈希表,带 SIMD 分组扫描 |
| [`kevy-resp`](crates/kevy-resp) | 零分配的 RESP2 / 3 parser |
| [`kevy-ring`](crates/kevy-ring) | 有界无锁 SPSC 队列 |
| [`kevy-madvise`](crates/kevy-madvise) | Linux `MADV_HUGEPAGE` 封装;其他平台是 no-op |
| [`kevy-uring`](crates/kevy-uring) | 纯 Rust io_uring 绑定 —— 不链接 liburing |
| [`kevy-geo`](crates/kevy-geo) | 地理空间命令原语 |
| [`kevy-lua`](crates/kevy-lua) | Lua 脚本桥接(基于 [luna](https://github.com/goliajp/luna) 运行时) |

其余 crate(`kevy-store`、`kevy-rt`、`kevy-persist`、`kevy-sys`、
`kevy-elect`、`kevy-replicate`、`kevy-scope`、`kevy-lua-host`、
`kevy-chaos`、`kevy-bench`、`kevy-pubsub-bench`)是服务器和嵌入式
库的内部基础设施 —— 之所以发布它们是为了让 workspace 能可复现地
构建,但终端用户通常用的是上面那些表面。

## 主题指南

| 主题 | 文档 |
|---|---|
| 配置调优 | [`docs/tuning.md`](docs/tuning.md) |
| 持久化(AOF + RDB) | [`docs/persistence.md`](docs/persistence.md) |
| Pub/Sub | [`docs/pubsub.md`](docs/pubsub.md) |
| 复制 | [`docs/replication.md`](docs/replication.md) |
| Cluster 模式 | [`docs/cluster.md`](docs/cluster.md) |
| Lua 脚本 | [`docs/lua.md`](docs/lua.md) |
| Unix 域套接字 | [`docs/uds.md`](docs/uds.md) |
| 异步客户端 | [`docs/async.md`](docs/async.md) |
| WebAssembly 构建 | [`docs/wasm.md`](docs/wasm.md) |
| Accept-shard 容量规划 | [`docs/accept-shards.md`](docs/accept-shards.md) |
| 错误回复参考 | [`docs/error-replies.md`](docs/error-replies.md) |

## 不在范围

kevy 对自己不做什么很诚实。按章程,以下事项**永久**不在范围内,
也没有添加计划:

- **AUTH 和 TLS。** kevy 假设运行在可信网络上。如果需要任一,
  请在前面放一个 TLS 终止 sidecar(envoy、stunnel)和一个认证代理。
- **多 DC active-active 与跨 DC 复制。** 仅单 DC。
- **多数据库 `SELECT`。** 一台服务器只有一个 keyspace。
- **ACL。** 单一信任域。
- **Gossip 发现与在线 resharding。** Cluster 拓扑是声明式的;
  resharding 是离线的。

如果上述需求里有任何一项是必须的,Redis Cluster、Valkey 或者一个
托管的 KV 服务才是合适的选择。

## 构建与测试

```sh
cargo build --workspace --release
cargo test  --workspace
```

Stable Rust 1.95,Rust 2024 edition。在 Linux(`x86_64`、`aarch64`)
和 macOS 上构建。`kevy-embedded` 及其依赖闭包也可以构建到
`wasm32-unknown-unknown` 与 `wasm32-wasip1`。

## 路线图与稳定性

workspace 当前处在 v2.x 线上。持久化格式、RESP 线协议、公开的
Rust API、CLI 参数、环境变量、TOML schema 以及驱逐语义在每条
主线内**只增不减**:由 v2.0 写出的文件可以被之后所有 v2.x 构建
加载,新增功能在 minor 发布里落地、不破坏既有代码。完整的稳定性
契约见
[`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md#v1x-stability-commitment)。

## License

按 MIT 或 Apache-2.0 二选一授权,由你选择。

© 2026 GOLIA K.K.
