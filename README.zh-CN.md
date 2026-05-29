# kevy

[English](README.md) · **简体中文** · [日本語](README.ja.md)

[![CI](https://github.com/goliajp/kevy/actions/workflows/ci.yml/badge.svg)](https://github.com/goliajp/kevy/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)
![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)

纯 Rust、**零依赖**、兼容 Redis 的键值服务器 —— 以硬件允许的最快速度运行。

kevy 使用 Redis 线协议（RESP2），因此 `redis-cli`、`valkey-cli` 以及所有
Redis 客户端库都能**无需改动**直接连接。底层引擎是完全用 Rust 编写的现代
thread-per-core、shared-nothing 架构 —— 唯一触及的 C 是无法回避的操作系统
系统调用边界。

```sh
cargo run -p kevy --bin kevy --release      # 仅 loopback，AOF 开启，端口 6004
redis-cli -p 6004 SET hello world
```

## 性能

> 超越 valkey 9.1 只是下限，不是目标 —— kevy 瞄准的是硬件天花板。

在一台专用 16 核 Linux 机器上测得（服务端用 0–9 核，客户端用独立核）：

| 指标 | kevy (io_uring) | valkey 9.1 (io-threads) | 倍率 |
|------|----------------:|------------------------:|-----:|
| **-c50 SET / 秒** | **4.0 M** | 1.5 M | **2.67×** |
| **-c50 GET / 秒** | **4.0 M** | 1.7 M | **2.33×** |
| -c1 SET / 秒 | 88 k | 58 k | 1.52× |
| -c1 GET / 秒 | 80 k | 65 k | 1.25× |

对比 io_uring 的 C 参考实现：**kevy 手写的 io_uring 绑定达到 148 ns 的
nop 往返，而 liburing 2.9 是 152 ns** —— 已贴着 Linux 内核底线，且没有链接
liburing。每个核心库 crate 的基准都达到或优于最强开源
Rust / Go / C / C++ 竞品的噪声地板水平（8 / 8）。

完整方法与复现步骤见 [`bench/REPORT.md`](bench/REPORT.md)。

## 为什么选 kevy

- **零 crates.io 依赖。** 只有 `std` 加 kevy 自己的 crate。每一个 hashmap、
  hash 函数、协议解析器都是 Rust 自研；唯一的 C 是操作系统边界（socket、
  epoll / io_uring、mmap），在单个 crate 里用 `unsafe extern "C"` 手写绑定。
- **Thread-per-core、shared-nothing。** 每个核心一个 reactor 加一个 keyspace
  分片，热路径上无锁；核心之间通过消息传递协调。
- **直接兼容 Redis。** RESP2 线协议，与 valkey 9.1 达成 94 条命令对等 ——
  redis-rs、go-redis、jedis、ioredis 等客户端无需改代码即可使用。
- **持久化。** 快照 + 追加写文件（AOF），`appendfsync` 支持
  `always` / `everysec` / `no`，语义与 Redis 一致。
- **现代数据结构**，而非 Redis 的遗留编码 —— 五种数据类型全部从零重写。

## 快速上手

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
**pub/sub**、**事务**（`MULTI` / `EXEC` / `DISCARD`）、持久化
（`SAVE` / `BGSAVE` / `BGREWRITEAOF`）和运维命令（`INFO` / `CONFIG` /
`CLIENT` / …）。多键命令和 pub/sub 都能跨每核分片工作，`WRONGTYPE` 的行为
与 Redis 一致。

带 valkey 对等说明的完整 94 条命令列表见
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
