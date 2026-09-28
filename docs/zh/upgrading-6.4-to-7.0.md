# 从 6.4 升级到 7.0

一句话版本：**走协议的客户端不用改代码，数据目录原样打开。** 升主版本是因为 Rust 这边的两处改动：`kevy-config` 的配置段结构体加了字段；kevy-cli 的工具只认 `--kevy` 写法，这是 6.x 期间就预告过的。其余需要看一眼的都列在下面：嵌入式存储可能在目录里留下的两种新文件、索引大小现在按实际报告，还有几个回复多了字段。

```toml
kevy-embedded = "7.0.0"
```

7.0.0 主要做了四件事：嵌入式存储在进程被杀时保住每一次已经返回的写入，打开和关闭也更快；全局索引，按值把一个索引分到各个 shard；索引的每一行只占原来的一半不到；加密链路，包括节点之间的链路和第二个客户端端口，不配置就不开启（见 [encrypted-links.md](encrypted-links.md)）。它还修了几个从 3.0 起就在丢数据的缺陷，见 [§9](#9-修掉的丢数据缺陷)。

## 一句话对照表

| 如果你…… | 会变什么 | § |
|---|---|---|
| 跑服务器，或者走协议访问 kevy | 换二进制，别的什么都不用 | — |
| 可能退回 6.4 | 先用 7.0 干净地打开并关闭一次；全局索引要重新声明 | 1 |
| 给索引设了 `MAXMEM`，或者分层存储的容量贴着索引下限 | 索引大小的读数是原来的两到四倍 | 2 |
| 按位置解析 `IDX.LIST` 或 `IDX.DESCRIBE` | 各多了一对 `partitioning` | 3 |
| 读嵌入式存储的变更流或 AOF | 一次 `MSET` 变成每个 shard 一帧 | 4 |
| 不小心在同一端口起了两个服务器 | 第二个现在会拒绝启动 | 5 |
| 实现 `kevy_rt::Commands` | 多了三个方法，都有默认实现 | 6 |
| 用结构体字面量构造 `kevy_config` 的结构体 | 写上新字段，或者用 `..Default::default()` | 7 |
| 脚本里以裸词调用 `kevy-cli doctor`、`export`、`sql compile` 等 | 把工具写到 `--kevy` 后面 | 8 |
| 想让一次索引读取只碰更少的 shard | 把它声明成全局索引 | [索引](indexes.md#全局索引partition-global) |

---

## 1. 退回 6.4：目录里可能有什么

以前进程被杀时，还在用户态缓冲区里等着的写入会丢。7.0 让每次追加在返回的那一刻就落在内核持有的内存里：

- **在 Apple 平台上**，映射的是 AOF 本身，末尾预分配一段（4 MiB 起，翻倍到 64 MiB 为止），追加就是往里复制。存储打开期间文件比其中的记录长，多出来的部分全是零；干净关闭时会截掉。被杀的进程会留下这些零，下次打开时清掉。
- **在其他平台上**，追加先进一个暂存环——一个小的映射文件 `aof-<i>.aof.stage`（默认 4 MiB），每个 tick 排进 AOF。下次打开时会重放被杀的进程留在里面的内容；在原目录里是这样，在被杀之后复制出来的目录（备份就是这么做的）里也一样。

6.4 两样都不认识。崩溃或被杀之后，先用 7.0 打开一次目录并干净地关闭，再退回 6.4：这样暂存环会排空、零尾会截掉，留下的文件 6.4 照旧能读。`Config::with_stage_ring(0)` 和 `Config::with_mapped_aof(false)` 可以关掉这两样；`AppendFsync::Always` 两样都不用。

日志里还有两处是新的，服务器和嵌入式存储都一样。流消费组里消费者的每次接触会记成内部的 `XINTERNAL.CONSUMERSEEN` 帧，6.4 会跳过它们——于是丢掉只由 `XGROUP CREATECONSUMER` 创建的消费者。另外，含有全局索引的目录会写成 6.4 读不了的 sidecar 格式，6.4 的服务器拿到它会以没有任何索引的状态启动（表编译出的路径也不在），直到重新声明。

## 2. 索引大小按实际报告

`IDX.LIST`、`IDX.VERIFY` 和 `TABLE.VERIFY` 里的 `bytes` 以前是每行 `值 + 键 + 48`，少算了堆。现在报的是行的实际开销：本地索引每行约 `键 + 字符串值 + 58…69` 字节——而行本身只有 6.4 的一半不到，所以读数比以前大，内存反而更少。同一个数字也用在索引的 `MAXMEM` 和分层存储给索引的预留上，因此：

- 用很紧的 `MAXMEM` 声明的索引，现在可能以 `-INDEXOVERBUDGET` 构建失败；
- 容量贴着索引下限的分层存储，保持热的数据会变少，或者按名字拒绝新索引。

在装了真实数据的样本上对照 `IDX.LIST` 重新核一下这些预算。每行的公式见 [indexes.md](indexes.md#一致性与成本模型)。

## 3. `IDX.LIST` 与 `IDX.DESCRIBE` 各多一对

- `IDX.LIST` 的每一行末尾多了 `partitioning local|global`；全局索引还有 `partitions`、`max_entries` 和 `mean_entries`。
- `IDX.DESCRIBE` 在 `declaration` 之前给出 `partitioning`，`declaration` 仍是最后一对。

按键读这些回复的客户端会看到可以忽略的新键；按位置数的就不行了。

## 4. 嵌入式 `MSET` 每个 shard 一帧

嵌入式存储以前对 `MSET` 的每一对各加一次锁、各记一条 `SET`。现在一个 shard 的所有对在一把锁下设置，记成一条 `MSET` 帧，所以崩溃时每个 shard 的那一份要么全在要么全不在。AOF、副本和变更流原来看到的是每个键一条 `SET`，现在看到的是 `MSET` 帧；只处理 `SET` 的变更流消费者也要处理 `MSET`，面对服务器时本来就得这样。

## 5. 已被占用的端口会被拒绝

每个 shard 都用 `SO_REUSEPORT` 监听，这让同一用户在同一端口上起的第二个 kevy 并进了第一个的监听：两个都在跑，各分走一部分连接，写入看起来会从客户端读回的那一个上消失。现在端口被占用时启动会以 `Address already in use` 停下，和 Redis 一样。

## 6. 给嵌入运行时的 Rust 用户

`kevy_rt::Commands` 多了 `take_ext_out`、`apply_ext` 和 `extension_targets`，默认实现都保持 6.4 的行为。它们在 shard 之间传递消息（一次写入要等这些消息都应用完才回复），并让一次扩展读取点名它需要的 shard。全局索引就建在它们之上。

## 7. `kevy-config`：配置段结构体多了字段

加密链路和能放在代理后面的集群带来了新配置项，而每个配置项都是一个公开字段：

| 结构体 | 新字段 |
|---|---|
| `Config` | `secure`（类型为 `SecureSection`：`private_key_file`、`listen_port`、`client_keys`、`cluster_port_base`、`announce_cluster_port_base`）|
| `ClusterSection` | `announce_ip`、`announce_port_base`、`secure`、`peer_keys` |
| `PeerEntry` | `repl_port_base` |
| `ReplicationSection` | `secure`、`upstream_key`、`replica_keys` |

把每个字段都写出来的结构体字面量会编译不过。要么写上新字段，要么在字面量末尾加 `..Default::default()`：上表里除了 `PeerEntry` 都有默认值，`PeerEntry` 的新字段写 `repl_port_base: None` 就是原来的行为。用 `Config::load` 或 `Config::from_toml_str` 得到的配置不用改，新配置项默认都是关闭的。

## 8. kevy-cli：工具只在 `--kevy` 后面

kevy-cli 6.4 以裸词运行工具，例如 `kevy-cli doctor -p 6004`、`kevy-cli export …`、`kevy-cli sql compile f.sql --apply --url h:p`，整个 6.x 期间每次运行都会打印一行提示，给出 `--kevy` 写法。7.0 里裸词就是服务端命令，和 redis-cli 一样，所以旧写法会把 `DOCTOR` 发给服务端，然后打印服务端的报错。把工具移到 `--kevy` 后面，连接选项放在它前面：

```sh
kevy-cli -p 6004 --kevy doctor
kevy-cli -p 6004 --kevy sql compile schema.sql --apply   # --url h:p 改成 -h h -p p
kevy-cli -p 6379 --kevy diff 127.0.0.1:6380 user:        # 第一个服务端就是会话连的那个
```

`digest <前缀>`，以及带文件参数的 `backup`/`restore`，改成 `--kevy digest|backup|restore`。为旧写法服务的库函数（`route_tool`、`run_doctor_cli`、`run_shadow_cli`、`run_lint_cli`、`run_backfill_keys_cli`）也一起删了。

## 9. 修掉的丢数据缺陷

下面每一项都可能在没有任何报错的情况下丢掉一次写入或一个截止时间：

- 后台 AOF 重写之后，哈希字段自己的 TTL（3.0.0 起）；
- 进程在事务中途死掉后写入的数据，在再下一次重启时丢失（4.0.0 起）；
- AOF 重写最后交换文件的那一刻写入的数据（5.0.0 起）；
- `BGSAVE` 之后写入的数据，在 macOS 和不用 io_uring 的 Linux 上（5.1.0 起）；
- `GETEX key EX|PX` 与带条件的 `HEXPIRE`，截止时间在重启时会移动；
- RESP3 下带数量的 `SPOP`，记录时没有带上它移除的成员；
- 多于一个 shard 的嵌入式副本上的大多数键，它们被写进了读取不会去找的 shard。

[changelog](https://github.com/goliajp/kevy/blob/develop/CHANGELOG.md) 里有每一项的完整说明。

---

## 获取

```toml
# Cargo.toml
kevy-embedded = "7.0.0"
```

```sh
npm install @goliapkg/kevy@7.0.0          # wasm
npm install @goliapkg/kevy-node@7.0.0     # Node 原生
pip install kevy==7.0.0
go get github.com/goliajp/kevy-go/v7@v7.0.0
cargo install kevy --version 7.0.0        # 服务器二进制，从源码编
npm install -g @goliapkg/kevy-bin@7.0.0   # 服务器二进制，预编译
```

Linux（x86-64、arm64）和 macOS（arm64）的预编译服务器二进制，各附一个 SHA-256 文件，在 [v7.0.0 release](https://github.com/goliajp/kevy/releases/tag/v7.0.0) 上。用 `ghcr.io/goliajp/kevy:latest` 的容器用户下次拉取就是 7.0.0；固定版本的 tag 是 `ghcr.io/goliajp/kevy:7.0.0`。
