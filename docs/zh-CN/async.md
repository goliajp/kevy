# 异步客户端(`kevy-client-async`)

`kevy-client-async` 是 [`kevy-client`](https://docs.rs/kevy-client) 的
runtime-agnostic 异步对应物。阻塞客户端仍是新项目的默认选择(纯 Rust、
零依赖,且在非异步工作负载下延迟更低)。这个 crate 给那些已经有
`tokio` / `smol` / `async-std` runtime、希望把 `await` 流贯穿到底的应用
使用 —— 特别是 pipelining,这才是 async 能把 N 次 round-trip 折叠成一
次的地方。

## 什么时候用哪个

| 你的场景 | 选 |
|---------|----|
| 没有 runtime,简单 request-response 代码 | `kevy-client` |
| tokio 应用,想每条命令一次 `await` | `kevy-client-async` |
| tokio 应用,想每批命令一次 `await` | `kevy-client-async` + `pipeline()` |
| 任何 runtime,嵌入式 `mem://` / `file://` | `kevy-client` |

`AsyncConnection::open` 会拒绝 `mem://` 和 `file://` URL —— 那些是进程
内同步后端;阻塞客户端对它们严格更快。

## Runtime 选择

`tokio` / `smol` / `async-std` 三个 feature 必须**正好启一个**。启零个
或两个以上都触发编译期错误。

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["tokio"] }
```

每个 runtime 有自己的 `TcpStream` adapter:

| feature | 传输 |
|---------|------|
| `tokio` | `tokio::net::TcpStream` |
| `smol`  | `smol::net::TcpStream` |
| `async-std` | `async_std::net::TcpStream` |

每个 runtime 依赖都开 `default-features = false` 加 adapter 所需的最小
surface feature。

## 表面 —— 阻塞客户端的镜像

```rust
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
```

`AsyncConnection` 上的方法名跟 `kevy_client::Connection` 1:1 一致(只
是多 `.await`)。从阻塞迁移就是 grep-replace `Connection` →
`AsyncConnection` 加 `.await` 每个调用。

可用命令族(42 个方法):

- **string + generic**:ping / set / get / del / exists / incr /
  incr_by / expire / persist / ttl_ms / type_of / dbsize / flushall /
  set_with_ttl / mget / mset / publish
- **hash**:hset / hget / hdel / hlen / hgetall / hkeys / hvals
- **list**:lpush / rpush / lpop / rpop / llen / lrange
- **set**:sadd / srem / smembers / scard / sismember / sinter /
  sunion / sdiff
- **sorted set**:zadd / zrem / zscore / zcard / zrange

## Pipeline-first 语法糖

这才是 async 真正发力的地方 —— 每批命令单次 TCP round-trip,而不是每
条命令一次。

```rust
let replies = conn
    .pipeline()
    .set(b"k1", b"v1")
    .get(b"k2")
    .incr(b"counter")
    .run(&mut conn)
    .await?;
// replies:Vec<Reply>,每条入队命令一项,顺序对齐。
```

单条命令的错误以 `Reply::Error(_)` 形式落在返回的 `Vec` 里 —— 一条命
令报错不会撕掉整批。外层 `Err` 保留给连接级别失败(传输、畸形帧)。

builder 没列出的命令用 `push_raw(argv)`:

```rust
conn.pipeline()
    .push_raw(vec![b"CUSTOM".to_vec(), b"arg".to_vec()])
    .run(&mut conn).await?;
```

### 降级路径

`Pipeline::into_cmds()` 返回 `Vec<Vec<Vec<u8>>>` —— 原始 argv 批。如
果你需要回落到阻塞客户端,逐条喂进去:

```rust
let cmds = conn.pipeline().get(b"a").set(b"b", b"v").into_cmds();
// 阻塞 kevy_client::Connection 上:
// for cmd in &cmds { blocking_conn.codec_mut().request(cmd)?; }
```

## Cluster client

`AsyncClusterClient` 镜像 `kevy_client::ClusterClient`,用于 cluster
模式服务器 —— 每个 shard 一个 TCP 连接、每个 key 走 CRC16 路由,正
确路由下永不触发 `-MOVED`。

```rust
use kevy_client_async::cluster::AsyncClusterClient;

let mut c = AsyncClusterClient::connect("127.0.0.1", 6004).await?;
c.set(b"user:42", b"…").await?;
```

## Subscriber

`AsyncSubscriber` 镜像 `kevy_client::Subscriber` —— 一个已订阅的 RESP
连接不能发普通命令,所以它是跟 `AsyncConnection` 分离的类型。对阻塞形
状直接 drop-in,只是去掉了 socket 级 `set_read_timeout`(用你 runtime
的 timeout 原语:`tokio::time::timeout`、`async_io::Timer` 等)。

```rust
use kevy_client_async::subscriber::AsyncSubscriber;

let mut sub = AsyncSubscriber::open("tcp://127.0.0.1:6004", &[b"ch"]).await?;
let (channel, payload) = sub.recv_message().await?;
```

## 错误

每个 async 方法都返回 `std::io::Result<T>`,跟阻塞客户端使用同样的
`ErrorKind` 映射:

| 来源 | `ErrorKind` |
|------|------------|
| RESP `-ERR …` 回复 | `Other` |
| 意外的回复 variant | `Other` |
| 畸形 RESP 帧 | `InvalidData` |
| 读到中途的 EOF | `UnexpectedEof` |
| 坏 URL / 端口 / scheme | `InvalidInput` |
| TLS / AUTH / embed URL scheme | `Unsupported` |
| 原始 socket I/O | (原生 kind) |

更广的错误上下文 —— RESP 错误字符串、意外的 variant 名 —— 在
`io::Error` 的 message 里(`.to_string()` / `.into_inner()`)。

## 依赖规则豁免

`kevy-client-async` 是 kevy workspace 里**唯一**被允许吃 crates.io 依
赖的 crate。豁免按 crate + 按 dep 计:`tokio`、`smol`、`async-std`
是仅有的 3 个引入的 crate(各自在 `Cargo.toml` 里挂内联 `# EXEMPTION`
注释)。其他 workspace crate 都不能依赖 `kevy-client-async` —— 那会让
豁免传递性蔓延出去。完整理由在 v3-cluster RFC(F5)和
`feedback-pure-rust-no-c-principle.md` memory 里。

## 示例

- [`tokio_hello`](../../crates/kevy-client-async/examples/tokio_hello.rs)
  —— open + ping + set/get + del。
- [`pipeline`](../../crates/kevy-client-async/examples/pipeline.rs)
  —— 在一次 round-trip 里跑混合 batch。
