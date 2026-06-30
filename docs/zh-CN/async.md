# 异步客户端

`kevy-client-async` 是阻塞版 [`kevy-client`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-client) 的异步镜像 —— 同样的接口面、同样的 URL 门面,每个调用上加 `.await`。

## 何时需要

当你的应用已经跑在 `tokio`、`smol` 或 `async-std` 运行时上、且你想要端到端的 `await` 流时,选异步客户端:不必经线程池跳、不必 `spawn_blocking` 包裹、不必每连接一线程。如果你的代码路径是普通线程上的请求-响应,阻塞客户端更简单也延迟更低 —— 同步代码不必为"我同步"付异步税。

## 核心思路

通过 Cargo feature(`tokio`、`smol` 或 `async-std`)恰好挑一个运行时;crate 编译下来就是那个运行时的 `TcpStream` 适配器,别的都没。公开接口面与阻塞客户端 1:1 镜像 —— `AsyncConnection::open(url).await?`、`conn.set(k, v).await?`、`conn.get(k).await?` —— 因此从阻塞迁移就是 `Connection` → `AsyncConnection` 加每个调用一个 `.await`。一个 pipeline 构建器在延迟要紧时把 N 条命令塌成一次 TCP 往返。

## 实际示例

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
    let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
    conn.set(b"k", b"v").await?;
    let v = conn.get(b"k").await?;
    assert_eq!(v.as_deref(), Some(&b"v"[..]));
    Ok(())
}
```

### Smol

代码不变,只换运行时 feature。

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["smol"] }
smol = "2"
```

```rust
use kevy_client_async::AsyncConnection;

fn main() -> std::io::Result<()> {
    smol::block_on(async {
        let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
        conn.set(b"k", b"v").await?;
        let v = conn.get(b"k").await?;
        assert_eq!(v.as_deref(), Some(&b"v"[..]));
        Ok(())
    })
}
```

### Pipeline 构建器

整批一次往返。回复按队列顺序返回;每条命令失败以 `Reply::Error(_)` 形式落在 `Vec` 里,而不是把整批撕掉。

```rust
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
let replies = conn
    .pipeline()
    .set(b"a", b"1")
    .get(b"a")
    .incr(b"hits")
    .run(&mut conn)
    .await?;
// replies.len() == 3; 每条入队命令一个 Reply,按顺序。
```

## 运行时 feature

下列特性必须恰好启用一个。零个、或两个及以上,都是编译期错误 —— 没有隐式默认。

| feature      | 传输适配器                          | 拉入的运行时 crate |
|--------------|-------------------------------------|--------------------|
| `tokio`      | `tokio::net::TcpStream`             | `tokio`            |
| `smol`       | `smol::net::TcpStream`              | `smol`             |
| `async-std`  | `async_std::net::TcpStream`         | `async-std`        |

每个运行时 crate 都以 `default-features = false` 加适配器所需的最小 surface 拉入。这是 kevy 工作区里仅有的 crates.io 依赖 —— 对"纯 Rust、零依赖"原则的一个有意保留的例外,因为 Rust 异步生态没有一个仅依赖 std 的可行底座。

## URL 后端

`AsyncConnection::open` 接受与阻塞客户端相同的 URL 门面。TCP 形态的 scheme 走运行时的异步 socket;进程内 scheme 被拒(对它们而言阻塞客户端严格更快 —— 经执行器路由没意义)。

| scheme       | 目标                                   | 异步客户端支持 |
|--------------|----------------------------------------|----------------|
| `tcp://`     | kevy 或 Redis 兼容服务器               | 是             |
| `kevy://`    | kevy 服务器(`tcp://` 别名)            | 是             |
| `redis://`   | Redis 或 Redis 兼容服务器              | 是             |
| `mem://`     | 进程内嵌入式 store                     | 否 —— 用阻塞客户端 |
| `file:///`   | 磁盘上的嵌入式 store                   | 否 —— 用阻塞客户端 |

对 `AsyncConnection::open` 用 `mem://` 或 `file:///` 会返回 `ErrorKind::Unsupported`。

## 取舍

阻塞客户端是默认,而且因为下列原因一直是默认:

- **同步代码路径**:如果你还没有运行时,别为了客户端立一个。`kevy-client` 是纯 Rust、零依赖,而且不为每条命令付执行器调度开销。
- **嵌入式后端**:`mem://` 与 `file:///` 是同步的进程内 store。阻塞客户端直接对它们说话;异步客户端做不到。
- **单发命令**:在标准多线程执行器上每命令一次 `.await` 相比直接 syscall 是可量的开销。异步的收益出现在并发(跨任务多个 in-flight 命令)或批量(pipeline 塌往返)上。

应用本身已是异步时用 async。一批独立命令、往返是瓶颈时用 pipeline 构建器。其它情形继续阻塞。

## FAQ

**为什么必须恰好选一个运行时?**
crate 编译单个 `TcpStream` 适配器。一个二进制里两个适配器要么意味着每次 IO 走运行时无关的间接(开销),要么意味着没人能维护的巨型 cfg 矩阵。零适配器又会让公开类型没有实现。编译期对 feature 数的检查让配置错误响亮且早发。

**我能在一个进程里混用同步和异步 kevy 客户端吗?**
能。`kevy-client`(阻塞)与 `kevy-client-async` 是独立 crate,自由共存 —— 比如同一个二进制里用阻塞对接嵌入式 `file:///` store,用异步对接网络上的 shard。它们不共享连接。

**pub/sub 怎么办?**
`AsyncSubscriber` 镜像阻塞的 `Subscriber`。已订阅的 RESP 连接不能发普通命令,所以它是与 `AsyncConnection` 独立的类型。每消息超时使用你运行时自己的原语(`tokio::time::timeout`、`async_io::Timer` 等),而不是 socket 级读超时。

**pipeline 构建器会强制发送侧缓冲吗?**
会 —— 这就是重点。`pipeline().…run(&mut conn).await` 把整批序列化成一次写,并按序列读 N 个回复。如果你要每条命令的反压,直接调 `set` / `get`,不要构 pipeline。

## 仓库内示例

- [`tokio_hello`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/tokio_hello.rs) —— open、ping、set/get、del。
- [`pipeline`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/pipeline.rs) —— 混合批一次往返。
