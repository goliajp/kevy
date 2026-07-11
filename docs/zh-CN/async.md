# 异步客户端

`kevy-client-async` 是阻塞版 [`kevy-client`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-client) 的异步镜像——接口面和 URL 门面完全相同，只是每个调用都带 `.await`。

## 什么时候需要它

如果应用本身已经跑在 `tokio`、`smol` 或 `async-std` 运行时上，想要端到端的 `await` 流，就选异步客户端：不用跳阻塞线程池、不用包一层 `spawn_blocking`、也不用每个连接开一个线程。反过来，如果代码路径就是普通线程上的请求-响应，阻塞客户端更简单、延迟也更低——同步代码没有理由白付一份异步税。

## 核心思路

通过 Cargo feature（`tokio`、`smol` 或 `async-std`）选定一个且只能选定一个运行时；整个 crate 编译出来就只有那个运行时的 `TcpStream` 适配器，再无其他。公开接口与阻塞客户端 1:1 对应——`AsyncConnection::connect(url).await?`、`conn.set(k, v).await?`、`conn.get(k).await?`——所以从阻塞版迁移过来，只需把 `Connection` 换成 `AsyncConnection`，再给每个调用加一个 `.await`。延迟敏感时还有 pipeline 构建器，可以把 N 条命令合并成一次 TCP 往返。

## 实例

### Tokio

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["tokio"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net"] }
```

```rust
use kevy_client_async::AsyncConnection;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut conn = AsyncConnection::connect("tcp://127.0.0.1:6004").await?;
    conn.set(b"k", b"v").await?;
    let v = conn.get(b"k").await?;
    assert_eq!(v.as_deref(), Some(&b"v"[..]));
    Ok(())
}
```

### Smol

代码不变，只换运行时 feature。

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["smol"] }
smol = "2"
```

```rust
use kevy_client_async::AsyncConnection;

fn main() -> std::io::Result<()> {
    smol::block_on(async {
        let mut conn = AsyncConnection::connect("tcp://127.0.0.1:6004").await?;
        conn.set(b"k", b"v").await?;
        let v = conn.get(b"k").await?;
        assert_eq!(v.as_deref(), Some(&b"v"[..]));
        Ok(())
    })
}
```

### Pipeline 构建器

整批命令只走一次往返。回复按入队顺序返回；单条命令失败会以 `Reply::Error(_)` 的形式留在 `Vec` 里，不会连累整批。

```rust
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::connect("tcp://127.0.0.1:6004").await?;
let replies = conn
    .pipeline()
    .set(b"a", b"1")
    .get(b"a")
    .incr(b"hits")
    .run(&mut conn)
    .await?;
// replies.len() == 3; one Reply per queued command, in order.
```

## 运行时 feature

下列 feature 必须启用且只能启用一个：一个都不开，或者同时开两个以上，都是编译错误——没有隐式默认值。

| feature      | 传输适配器                          | 拉入的运行时 crate |
|--------------|-------------------------------------|--------------------|
| `tokio`      | `tokio::net::TcpStream`             | `tokio`            |
| `smol`       | `smol::net::TcpStream`              | `smol`             |
| `async-std`  | `async_std::net::TcpStream`         | `async-std`        |

每个运行时 crate 都以 `default-features = false` 引入，只带适配器需要的最小 surface。它们是 kevy 工作区里仅有的 crates.io 依赖——“纯 Rust、零依赖”原则下特批的唯一例外，因为 Rust 异步生态没有只靠 std 就能用的底座。

## URL 后端

`AsyncConnection::connect` 接受与阻塞客户端相同的 URL 门面。TCP 形态的 scheme 走运行时的异步 socket；进程内 scheme 则直接拒绝（对它们来说阻塞客户端严格更快，绕道执行器没有意义）。

| scheme       | 目标                                   | 异步客户端支持 |
|--------------|----------------------------------------|----------------|
| `tcp://`     | kevy 或 Redis 兼容服务器               | 是             |
| `kevy://`    | kevy 服务器（`tcp://` 的别名）         | 是             |
| `redis://`   | Redis 或 Redis 兼容服务器              | 是             |
| `mem://`     | 进程内嵌入式 store                     | 否——用阻塞客户端 |
| `file:///`   | 磁盘上的嵌入式 store                   | 否——用阻塞客户端 |

对 `AsyncConnection::connect` 传 `mem://` 或 `file:///` 会返回 `ErrorKind::Unsupported`。

## 取舍

阻塞客户端是默认选择，而且会一直是默认，原因有三：

- **同步代码路径**：如果手上还没有运行时，不必为了客户端专门架一个。`kevy-client` 纯 Rust、零依赖，每条命令也不用付执行器的调度开销。
- **嵌入式后端**：`mem://` 和 `file:///` 是同步的进程内 store。阻塞客户端直接跟它们对话；异步客户端做不到。
- **单发命令**：在普通多线程执行器上，每条命令一次 `.await`，相比直接 syscall 的开销是可以测出来的。异步的收益出现在并发（多个任务各有 in-flight 命令）或批量（pipeline 合并往返）场景。

应用本身已是异步时，用异步客户端；有一批相互独立的命令、往返延迟是瓶颈时，用 pipeline 构建器；其余情况留在阻塞客户端。

## FAQ

**为什么必须选定且只能选定一个运行时？**
这个 crate 只编译一个 `TcpStream` 适配器。一个二进制里塞两个适配器，要么每次 IO 都走一层运行时无关的间接调用（开销），要么维护一个没人维护得动的巨型 cfg 矩阵；零适配器则公开类型没有实现。对 feature 数量做编译期检查，能让配置错误在编译期就大声报出来。

**同步和异步 kevy 客户端能在一个进程里混用吗？**
能。`kevy-client`（阻塞）和 `kevy-client-async` 是相互独立的 crate，可以随意共存——比如同一个二进制里，用阻塞客户端对接嵌入式 `file:///` store，用异步客户端对接网络上的 shard。两者不共享连接。

**pub/sub 怎么办？**
`AsyncSubscriber` 对应阻塞版的 `Subscriber`。已订阅的 RESP 连接不能再发普通命令，所以它是独立于 `AsyncConnection` 的类型。逐消息超时用你所在运行时自己的原语（`tokio::time::timeout`、`async_io::Timer` 等），而不是 socket 级读超时。

**pipeline 构建器会在发送侧强制缓冲吗？**
会——这正是它存在的意义。`pipeline().…run(&mut conn).await` 把整批命令序列化成一次写入，再按顺序读回 N 条回复。如果需要逐条命令的反压，就直接调 `set` / `get`，不要走 pipeline。

## 仓库内示例

- [`tokio_hello`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/tokio_hello.rs)——open、ping、set/get、del。
- [`pipeline`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/pipeline.rs)——混合批次一次往返。
