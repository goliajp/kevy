# 升级 kevy

两章，新的在前：**3.x → 4.0**（一次 API 定义型的 major：客户端线协议照旧，磁盘目录原样打开、格式在首次重写时升级，Rust 接口面改了这一次，此后冻结）和 **2.x → 3.x**（一次能力型的 major：一切照旧带过去）。每一章都明确写清：什么自动升级、什么需要改代码、以及怎么退回去。

---

# 3.x → 4.0

4.0 是工业生产级的宣告。这个 semver major 是刻意花掉的——用一次破坏窗口，把所有积欠已久的公开 API 债一次还清，此后接口面**冻结**：后续的 4.x 只做加法。如果你是通过线协议跟 kevy 说话，4.0 就是换个二进制。如果你链接了某个 kevy crate，预期是一次短促而机械的迁移——下面每一条变更都有一句话的规则。

## TL;DR——各组件版本一览

| 组件 | 3.x 时代 | 4.0 | 动作 |
|---|---|---|---|
| `kevy`（服务器） | 3.18.x | 4.0.0 | 换二进制，在同一个数据目录上重启 |
| `kevy-embedded` | 3.18.x | 4.0.0 | 升版本 + 照下面的 API 表改 |
| `kevy-client` | **1.14.x** | **2.0.0** | 升版本——两个客户端保持自己的版本线，这次破坏性变更对应它们的 2.0.0 + 那张 API 表 |
| `kevy-client-async` | **1.1.x** | **2.0.0** | 同上 |
| `kevy-wasm` / `@goliapkg/kevy`（npm） | —— | 4.0.0 | 4.0 新增——见 [wasm.md](wasm.md) |
| 基础设施 crate（`kevy-store`、`kevy-rt`……） | 3.18.x | 4.0.0 | 跟随工作区版本 |

`kevy-client` 与 `kevy-client-async` **不在**工作区的版本线上：它们发的是 **2.0.0**，不是 4.0.0。`cargo add kevy-client` 解析到 2.x 就是当前版本，不是过期版本。

## 什么是自动兼容的

**线协议。** RESP 没变，每一个 verb 别名（`SLAVEOF`、`HMSET`……）都保留。Redis 客户端、脚本、`redis-cli` 会话照旧可用；对着 valkey 9.1 的回复对等性套件仍然在 CI 里把关。

**复制线协议（kevy ↔ kevy）——一处干净的断裂。** 内部复制握手现在携带 feed generation（`REPLICATE FROM <gen> <offset> ID <id>` / `+ACK <gen> <offset>`）——正是让 offset 续传在主节点 unclean 重启之后依然安全的那道栅栏。4.0 副本无法与 3.x 主节点握手，反之亦然；一个复制对的两端要在同一个窗口里升级（先副本、后主节点是停机最少的顺序——新主节点起来后副本重连并全量同步）。这只是 kevy 的内部协议；面向 Redis 客户端的一切都没变。

**快照与 AOF。** 4.0 的二进制能读每一种 3.x（以及 2.x）的快照格式，并原样重放 3.x 的 AOF——3.x 数据目录零迁移打开。4.0 的新东西是 AOF 的*记录格式*（`KEVYAOF2`：长度前缀 + CRC32C 校验的记录——见 [persistence.md](persistence.md)），升级按文件惰性进行、且单向：

- 向既有 3.x（`KEVYAOF1`）文件的追加保持 v1——换二进制的第一天，你的文件和 3.18 逐字节兼容，退回去仍然只是换回二进制。
- 新文件、以及**首次重写**（自动重写或 `BGREWRITEAOF`）的输出是 v2——3.x 读不了它。首次重写之后，同一数据目录上不再存在退回 3.18 的路（除非经客户端把键空间重放出去）。

如果想在灰度窗口里保留一条 3.18 退路：窗口期间关掉自动重写——服务端 `auto_aof_rewrite_percentage = 0`，嵌入式 `Config::with_auto_aof_rewrite_disabled()`（4.1，一次调用清掉全部三个触发旋钮）——并先做一份快照备份；灰度站稳后再开回来。CRC 保护要等文件升到 v2 才生效，所以别让这个状态超出灰度所需的时长。从 4.1 起这扇窗**可观测**而非靠推断：嵌入式 `Store::downgradeable_to_v3()`，服务端 `INFO persistence` 的 `aof_format:`。

**配置。** 每一个 3.x 配置键都被接受，含义不变。有两个键的语义变得**更严或更真**——见下面的“行为变更”（`notify_keyspace_events` 拒绝未知 flag、`min_replicas_max_lag_ms` 真正生效）。

**移除了一个旋钮。** 自定义快照 / AOF **文件名**没有了（`kevy-embedded` 的 `Config::with_snapshot_filename` / `with_aof_filename` 两个 builder）。磁盘布局现在固定为每 shard 一组 `dump-{i}.rdb` / `aof-{i}.aof`。用默认名字写出来的目录——包括每一个遗留的单文件目录——照样原样加载；只有用**自定义**名字写过的目录，需要在第一次用 4.0 打开之前，做一次性的 `mv` 改成固定名字。

## API 破坏对照表

本节里的一切都是编译期破坏，且都有机械的修法。除非明确点出，否则不改变任何运行期语义。

### 1. `flush()` 垫片移除

废弃的别名没有了；活下来的那个名字如实说出它干的事（它会**擦掉**整个 store）：

| Crate | 移除的 | 改调 |
|---|---|---|
| `kevy-embedded` | `Store::flush()` | `Store::flushall()` |
| `kevy-client` | `Connection::flush()` | `Connection::flushall()` |
| `kevy-store` | `Store::flush()` | `Store::flushall()` |

### 2. 单一错误货币：`KevyError`

`kevy-embedded` 和 `kevy-client` 的每一个公开可失败接口面，现在都返回 `KevyResult<T>`（即 `Result<T, KevyError>`），而不是 `io::Result<T>`。这个类型住在 `kevy-store` 里，两个 crate 都做了 re-export：

```rust
pub enum KevyError {
    Store(StoreError),      // structured engine errors, no longer
                            // flattened into io::Error strings
    Io(std::io::Error),     // real I/O, preserved via From
    Protocol(String),       // server error replies, wire text intact
    ReadOnly,               // replica write rejection
    InvalidInput(String),   // e.g. URL parse errors
    NotFound(String),
    Unsupported(String),    // e.g. remote-only calls on embedded
    TimedOut,               // e.g. Subscription::recv_timeout
    Closed,                 // stream/bus gone; also terminates
                            // subscriber iterators
}
```

迁移通常只是改一下返回类型标注——`?` 照常工作，因为 `From<io::Error>` 和 `From<StoreError>` 都在：

```rust
// 3.x
fn warm(conn: &mut Connection) -> std::io::Result<()> {
    conn.set(b"greeting", b"hello")?;
    Ok(())
}

// 4.0
use kevy_client::{Connection, KevyResult};

fn warm(conn: &mut Connection) -> KevyResult<()> {
    conn.set(b"greeting", b"hello")?;
    Ok(())
}
```

原本要**检查**错误的代码，处境严格变好了——匹配 variant，不必再解析字符串：

| 3.x 的信号 | 4.0 |
|---|---|
| `io::Error::other("kevy-store: …")` 的包裹文本 | `KevyError::Store(e)`——结构化的 `StoreError` |
| 副本拒绝写入：`io::Error::other("READONLY …")` | `KevyError::ReadOnly`（它的 `Display` 仍然以 `READONLY` 开头） |
| 服务器 `-ERR …` 回复表现为不透明的 `io::Error` | `KevyError::Protocol(text)`——线上文本原样保留 |
| `Subscription::recv_timeout` 抛出的 `ErrorKind::TimedOut` | `KevyError::TimedOut` |
| 用 `ErrorKind::UnexpectedEof` 表示订阅流已断 | `KevyError::Closed`；`SubscriberEvents` / `SubscriberMessages` 迭代器产出 `KevyResult<_>` 并在此结束 |
| 远端专有特性用的 `ErrorKind::Unsupported` | `KevyError::Unsupported(msg)` |

**从 4.1 起，`From<KevyError> for io::Error` 存在了**——按 kind 映射（`TimedOut → ErrorKind::TimedOut`、`Closed → ConnectionAborted`、OOM → `OutOfMemory`……），并且**保留 source**：类型化的 `KevyError` 作为 `io::Error` 的 source 随行，可以 downcast 取回，边界上什么都不丢。4.0 刻意不出这条回边，理由是它会复活有损降级；随后第一次生产迁移把这个转换手写了约 280 次 `io::Error::other(e)`——那才是有损降级，而且还没有 kind 映射。orphan 规则决定了只有 kevy 能提供这个 impl，所以 kevy 提供。困在 `io::Result` 世界里的函数现在直接 `?` 就行。

**迁移实际由什么构成——来自大规模做过这件事的消费者**：破坏点只有错误类型——`Store::open`、`Config` 和各方法形态都没变。机械配方：你拥有的可失败签名改成 `KevyResult`（通常只改注解，如上例）；你不拥有的地方，让新的 `From` 把 `?` 带进 `io::Result`。别一页一页追编译错误，把编译器当查询跑——`cargo check --message-format=json` 配 jq 得到的去重清单就是你的工单，它的长度就是你的估算。还有一条那位消费者绕了弯才学到的：错误数**不单调**——二进制 crate 要等它依赖的库编译过了才暴露自己的转换错误，所以循环要跑到不动点（一轮不再有变化），不是跑到某个计数。

有一个例外留在明处：CDC feed 的 embedded 接口面（`changes_tail` / `changes_since`）保留它自己的 `FeedError`——`Resync` 与 `Future` 是这条流独有的控制信号，不是通用错误。

（`kevy-resp-client` 有意保留 `io::Result` 接口面——它是一块纯传输的石头，`io::Error` 就是它诚实的货币。）

### 3. 构造函数命名：资源用 `open`，网络用 `connect`

每一类东西只有一个动词，处处如此。本地的、有文件支撑的东西用 `open`；有对端的东西用 `connect`；纯内存的值用 `new`。改名清单：

| Crate | 3.x | 4.0 |
|---|---|---|
| `kevy-client` | `Connection::open(url)` | `Connection::connect(url)` |
| `kevy-client` | `Subscriber::open(url, channels)` | `Subscriber::connect_channels(url, channels)` |
| `kevy-client-async` | `AsyncConnection::open(url)` | `AsyncConnection::connect(url)` |
| `kevy-client-async` | `AsyncSubscriber::open(url, channels)` | `AsyncSubscriber::connect_channels(url, channels)` |
| `kevy-resp-client` | `RespClient::from_url(url)` | `RespClient::connect_url(url)` |

本来就合规、原样不动的：`kevy_embedded::Store::open`、`kevy_persist::Aof::open`、`kevy_store::Store::new`、`ClusterClient::connect`、`RwClient::connect`、`Subscriber::connect(url)`、`RespClient::connect(host, port)`。

### 4. `kevy_rt::Runtime` 改为构建，不再靠位置传参

位置参数的构造函数没有了；`Runtime` 自己就是 builder：

```rust
// 3.x
let rt = Runtime::new([127, 0, 0, 1], 6004, 4, commands);

// 4.0
let rt = Runtime::builder(commands)
    .bind([127, 0, 0, 1], 6004)
    .shards(4);
```

`builder(commands)` 的默认值：bind `127.0.0.1:6004`、1 个 shard、AOF 开启（`EverySec`）、数据目录 `"."`。`bind` / `shards` 是 `#[must_use]` 的 setter，和既有的 `with_*` 链一致。这同时也是 4.0 实例化工作的可见面：`Runtime` 不再触碰任何全局状态，所以一个进程里可以跑好几个互相独立的 kevy 实例。

### 5. `kevy-store` 的写方法收借用 argv

持有所有权的那套形态被移除；借用形态（原来的 `_borrowed` 双胞胎）接管了正式名字：

| 3.x（持有所有权） | 4.0（借用，同名） |
|---|---|
| `del(&[Vec<u8>])` / `exists(&[Vec<u8>])` | `del(&[&[u8]])` / `exists(&[&[u8]])` |
| `hset(&[(Vec<u8>, Vec<u8>)])` / `hdel(&[Vec<u8>])` / `hmget(&[Vec<u8>])` | `hset(&[(&[u8], &[u8])])` / `hdel(&[&[u8]])` / `hmget(&[&[u8]])` |
| `sadd` / `srem` / `lpush` / `rpush` / `zrem` `(&[Vec<u8>])` | 同名，`(&[&[u8]])` |
| `zadd(&[(f64, Vec<u8>)])` | `zadd(&[(f64, &[u8])])` |
| `zadd_flags_borrowed(…)` | 改名为 `zadd_flags(…)` |

```rust
// 3.x
store.del(&[b"k1".to_vec(), b"k2".to_vec()]);

// 4.0 — pass slices; no allocation
store.del(&[b"k1".as_slice(), b"k2".as_slice()]);
```

这是一次穿着 API 变更外衣的性能修复：`kevy-embedded` 的门面现在把借用的 argv 直接递下去，embedded 写路径上每次调用一份的 `to_vec()` 拷贝没有了。

### 6. `Commands` trait + `Route`（自带命令集的嵌入方）

只有你自己 `impl kevy_rt::Commands` 时才相关：

- `dispatch_resp3`（返回 Vec 的那个形态）移除——改为覆写 `dispatch_into_resp3`。
- `wake_idx` 方法移除——改为在你的 `resolve()` 里填充 `ResolvedCmd::wake_idx`；那个字段本身没变。
- `extension_reduce_v3` 和旧的双参数 `extension_reduce` 合并为 `extension_reduce(argv, chunks, proto) -> ExtensionReduced`；返回 `ExtensionReduced::Reply(bytes)`，或者用 `ExtensionReduced::Continue(argv2)` 取代旧的那种 NUL 前缀的带内续帧。
- `Route::{MGet, SInter, SUnion, SDiff, ZInterCard}` 塌缩成 `Route::Gather(MultiOp)`；`Route::{Keys, Scan, RandomKey}` 塌缩成 `Route::Keyspace(KeyShape, Option<Vec<u8>>)`。`MultiOp` 和 `KeyShape` 新近公开。
- `Commands::on_replication_view` 的副本条目现在是 `(String, Ipv4Addr, u16, u64, Option<ReplicaAck>)`（类型别名 `ReplicaViewRow`）——打头是副本 id `String`，然后是对端的 `(Ipv4Addr, u16)`、已发送 offset，以及 `ReplicaAck { acked_offset, ack_age_ms }`（它取代了原来那个裸的 acked offset）；解构它即可。

## 行为变更（不用改代码，但运维可见）

- **`-LOADING` 是真的了。** 副本在吞下一份全量重同步快照期间，读取回答 `-LOADING`，而不是把替换到一半的数据集拿出来服务。`PING`、`INFO`、`HELLO` 仍然可答（健康检查照常工作），与 Redis 豁免的那批 verb 一致。为 Redis 写的“遇 `-LOADING` 重试”循环，原样就是正确的。
- **`notify_keyspace_events` 拒绝未知的 flag 字符**，在配置解析时就拒，而不是默默忽略——并且 flag 集合真的长出了 `x`（expired）、`e`（evicted）、`n`（new-key）事件。以前能把一个拼写错误偷渡过去的配置，现在会大声失败；把 flag 串改对即可。
- **`min-replicas-to-write` 只数活着的 ACK。** `min_replicas_max_lag_ms` 这个键在 3.x 就存在，现在真正生效了：上一次 ACK 早于该窗口的副本，不再满足写闸门。原本靠一个**卡死的**副本来维持写入畅通的部署，会看到 `-NOREPLICAS`——而那正是这个键一直以来所承诺的语义。
- **`CLIENT` 接口面说实话了**：`CLIENT LIST` 是真实的连接表（地址由 getpeername 支撑，id 全局唯一），`CLIENT KILL` 是真的杀（包括阻塞中的连接），`CLIENT SETNAME` 是真的留得住，`INFO` 里的 `connected_clients` 是一个实时读数。
- **`SHUTDOWN` 优雅排空**：在途的回复会写完，AOF 在退出前拿到最后一次 fsync，把从前一个裸 SIGTERM 会留下的那段 everysec 未同步尾巴关掉了。
- **`ROLE` / `INFO replication` 跨 shard 聚合**，并带逐副本的身份（`ip:port` 与真实的逐副本 offset）。原先假定“每台服务器一行汇总”的解析器照常工作；逐副本的那些行只是更丰富了。

## Feature 系统（4.0 新增）

`kevy-embedded` 现在按 feature 分档，好让小目标只为自己用到的东西付钱。默认仍然是全都开：

| Feature | 加进来什么 | 拉进哪些 crate |
|---|---|---|
| `core` | KV + TTL + pubsub + 原子块 / pipeline | （无） |
| `persist` | 快照 + AOF | `kevy-persist` |
| `index` | 声明式索引 + 视图 | `kevy-index` |
| `text` | 全文（BM25）段 | `index`、`kevy-text` |
| `vector` | HNSW ANN 段 | `index`、`kevy-vector` |
| `replicate` | 复制 + CDC feed | `persist`、`kevy-replicate` |
| `listener` | 只读 RESP listener | （无） |

`core` 档可以交叉编译到 musl 目标，并扛着一份强制预算（二进制 ≤ 700 KB，空 store RSS ≤ 2 MB）；另有五个基础 crate 能构建 `no_std`。见 [iot.md](iot.md)。在体积谱的另一端，同一个 embedded 内核现在能以 `@goliapkg/kevy` 的身份跑在浏览器里——见 [wasm.md](wasm.md)。

## 从 4.0 退回 3.18

换回二进制即可；快照和 AOF 格式是共享的。唯一的边缘：配置文件里用了新的 notify flag（`x` / `e` / `n`）时，3.18 能解析它，但那些事件在那边永远不会触发。

---

# 2.x → 3.x

kevy 3.x 是 2.x 的超集：每一个 2.x 负载原样可跑，服务器侧的升级是换个二进制，embedded 用户则是升一下依赖。这一章明确写清：什么自动带过去、什么改了名字或数字，以及唯一需要小心的那个方向（退回 2.x）。

## TL;DR——各组件版本一览

| 组件 | 2.x 时代 | 3.x（终版 3.18.x） | 动作 |
|---|---|---|---|
| `kevy`（服务器） | 2.0.x | 3.18.x | 换二进制，在同一个数据目录上重启 |
| `kevy-embedded` | **1.x**（1.4–1.16） | **3.x** | 升依赖——1.x 这条线在 v3.0.0 整个工作区统一到同一个版本时终结 |
| `kevy-client` | 1.12.x | 1.13–1.14 | 升版本；API 不变 |
| `kevy-client-async` | 1.0.x | 1.1.x | 升版本；API 不变 |
| `kevy-cli` | 未发布 | 3.x | `cargo install kevy-cli`——现在带着整套迁移工具链 |
| 基础设施 crate（`kevy-store`、`kevy-rt`……） | 2.0.x | 3.x | 跟随工作区版本 |

`kevy-embedded` 从 1.x 跳到 3.x 是一次**版本线统一，不是 API 重写**：1.16 的接口面完整包含在 3.x 里。如果你的 `Cargo.toml` 写的是 `kevy-embedded = "1"`，改成 `"3"` 重新编译即可——或者直接照着上面那一章去 `"4"`。

## 什么是自动兼容的

**线协议。** RESP 没变。3.x 在 CI 里仍然对着 valkey 9.1 做逐字节的回复核对（98 个命令）。现有的 Redis 客户端、脚本、`redis-cli` 会话照旧可用。

**快照。** 3.x 的加载器能读每一种 2.x 快照格式（`KEVYSNAP` 版本 2–5）：相对 TTL 的 v2 文件、绝对 TTL 的 v3、带 stream group 的 v4、带 feed 游标的 v5。把一台 3.x 服务器指向一个 2.x 数据目录，它加载得起来。

**AOF。** AOF 是一份 verb 日志，而 3.x 的 verb 集是 2.x 的超集——重放原样可用。`appendfsync` 语义不变。

**配置。** 每一个 2.x 配置键都被接受。新加的段（`[replication] single_source`、`--accept-shards`……）都是加法，默认值复现 2.x 的行为。

## 升级步骤

### 服务器部署

1. 在还在跑的 2.x 上拍一份快照（`SAVE` 或者你平常的备份方式），并留一份副本——为什么要留，见下面的“退回”。
2. 停掉 2.x，用同样的参数和数据目录启动 3.x 二进制。
3. 校验：`DBSIZE` 对得上；想要密码学级别的把握，就在升级前后各跑一次 `kevy-cli digest -p <port> <prefix>`——digest 相等即代表键空间完全一致。

滚动升级一对副本：先升副本，让它重新同步，然后把流量切过去，再升原来的主节点。（2.x 没有托管切主，所以从 2.x 出发，这就是常规的手工切换。一旦到了 3.15+，切主这一步本身就变成一个 verb——`FAILOVER host port`，见 [availability.md](availability.md)。）

### Embedded 应用

1. `Cargo.toml` 里写 `kevy-embedded = "3"`。
2. 重新编译。1.16 的 API 原样都在；新的能力面（索引 / 视图 / 全文 / 向量 / feed / 复制）是加法形式的方法和 `Config` 选项。
3. 有一条 trait 注意事项：当且仅当你自己写了 `impl kevy_rt::Commands` 并且用字面量构造 `ResolvedCmd` 时，v2 那一段加了两个字段（`block_hint`、`wake_idx`）。默认的 `resolve()` 会填好它们；字面量构造则要把这两个字段补上。
4. 一个 1.x embedded 应用留在磁盘上的数据原样可加载（快照格式与服务器相同）。

### 客户端

`kevy-client 1.13+` / `kevy-client-async 1.1` 是直接替换：这次 minor 升版只是把内部 crate 重新钉到 3.x 工作区。通用 Redis 客户端库不受任何影响。

## 3.x 加了什么（你为什么要升）

带补水的声明式索引（`IDX.*`）、具名视图（`VIEW.*`）、写入时聚合（GROUP BY / 分布式 top-K）、无词典的 CJK 全文检索加 BM25、HNSW 向量 KNN（外加 BM25 + KNN 的混合融合）、带恢复点契约的 CDC feed（`FEED.*`）、embedded 作为主节点的复制、机器可读的契约（`COMMAND DOCS`、生成式参考文档、`kevy-mcp` 这个 MCP 服务器）、可用性这条弧（复制滞后真值、`FAILOVER`、多数派崩溃选举，以及 `WAIT` / `REPL.TOKEN` / `REPL.WAIT` 一致性阶梯——见 [availability.md](availability.md)），还有迁移工具链（`kevy-cli import/export/--verify/diff/inspect/digest`）。从 [designing-on-kevy.md](designing-on-kevy.md) 和 [cookbook.md](cookbook.md) 开始读；性能凭据在 [bench/PERF-LEDGER.md](https://github.com/goliajp/kevy/blob/develop/bench/PERF-LEDGER.md)。

这些没有一个是隐式生效的：一台跑着 2.x 负载的 3.x 服务器，目录是空的，而空目录上的索引钩子被 perfgate 棘轮盯着（相对 2.x 无回归）。

## 退回（唯一需要小心的方向）

3.x 服务器**写出**的是快照格式 v4；一旦存在 CDC feed 游标，写出的就是 v5。而 2.x 二进制最多只读到 v4：

- 如果你从没开过 feed，一份 3.x 快照在 2.x 上加载得起来。
- 如果 feed 曾经激活过（v5），2.x 会拒绝这个文件。退回路径：在 3.x 上 `kevy-cli export` → `kevy-cli import` 进一个全新的 2.x——或者拿出第 1 步那份升级前的备份，接受这中间的数据缺口。

3.x 里引入的 verb（`IDX.*`、`VIEW.*`、`FEED.*`……）在 2.x 二进制上自然重放不了——如果你用过它们，正确的退回路径是 export / import，不是 AOF 重放。

## 版本历史，每个一句话

- **3.0.0**——服务引擎的宣告（索引、视图、全文、ANN、CDC、上车坡道；十一列受 gate 的火车）。
- **3.8.0**——性能弧（对着 valkey 9.1 和 RediSearch 实测；裸接口面 1.6–3.3×，ANN 在召回 1.000 时领先 1.64×，FTS 单个常见词 93×；embedded 作为主节点的复制）。3.0.0 到 3.8.0 之间没有切过版本；3.8.0 包含 v3.1–v3.8 这几列火车。
- **3.17.0**——可用性版本：AI 原生的服务接口面（机器可读的 verb 契约、生成式文档、`kevy-mcp`、混合检索）与可用性弧（复制心跳 / ACK 真值、`FAILOVER` + 多数派崩溃选举、一致性阶梯、CI 里的契约 gate）。包含 v3.9–v3.17 这几列火车；中间没有切过版本。
- **3.17.1–3.17.4**——维护：`luna-core` Lua 运行时升级、文档 / 迁移那一波、首批采用者的反馈（`kevy-cli --embed`），以及文档 / i18n 的打磨波。
- **3.18.0**——结构版本：LOC 债务清零且上限在 CI 里强制执行、又有六块石头做了 fuzz（首日收获：修掉四个真 bug）、miri / pedantic / missing-docs 扫荡、Rust 1.97.0。
