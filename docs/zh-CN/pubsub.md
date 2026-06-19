# kevy-client 里的 Pub/sub

同一份代码同时驱动进程内总线和 TCP kevy 服务器。运行时用 URL 选后端
—— 调用现场不需要 scheme-branch。

```toml
[dependencies]
kevy-client = "1.11"
```

## URL 语义

| URL | 后端 | 跨多次 open 共享? |
|-----|-----|------------------|
| `mem://` | 进程内、纯内存 | **否** —— 每次 open 都新建 |
| `mem://<name>` | 进程内、纯内存 | **是** —— 同 `<name>` 同总线 |
| `file:///abs/path` | 进程内 + snapshot/AOF 持久化 | **是** —— 同 path 同总线 |
| `kevy://host[:port][/db]` | TCP kevy/Redis 服务器 | (每 open 一个 socket,服务器侧扇出) |
| `redis://host[:port][/db]` | TCP —— `kevy://` 别名 | 同上 |
| `tcp://host[:port]` | TCP —— 裸,无前置 `SELECT` | 同上 |

`rediss://` / `kevys://` / `redis://user:pass@…` 都被
`ErrorKind::Unsupported` 拒绝 —— kevy 不带 TLS / AUTH。

**匿名 `mem://` 收不到 publish 消息**,因为没东西能到达同一个底层
`Store`。`Subscriber::open` 用 `ErrorKind::Unsupported` 拒绝它。用
`mem://<some-name>`。

**集群说明**:kevy 里的 pub/sub 是**进程级**,不走 slot 路由:任何
cluster shard 端口上 publish 都能到达同进程其他 shard 端口的订阅者。
你**不**需要 `ClusterClient` 给 pub/sub —— 一个普通的
`Connection::open("kevy://host:port")` 连任一 shard 端口都行。看
[`docs/cluster.md`](cluster.md) 关于 slot 路由的 keyspace 流量。

## Pattern 1 —— 同线程 dev 循环

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub  = Subscriber::open("mem://app", &[b"news"])?;
let mut conn = Connection::open("mem://app")?;

// publish 前排空 SUBSCRIBE ack —— bus 有序,但 ack 排在第一个
// Message 前面。
let _ack = sub.recv()?;

conn.publish(b"news", b"hello")?;

if let PubsubEvent::Message { channel, payload } = sub.recv()? {
    assert_eq!(channel, b"news");
    assert_eq!(payload, b"hello");
}
# Ok::<(), std::io::Error>(())
```

## Pattern 2 —— 跨线程 producer / consumer

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};
use std::thread;

const URL: &str = "mem://orders";

let mut sub = Subscriber::open(URL, &[b"order.placed"])?;
let _ack = sub.recv()?;

thread::spawn(|| {
    let mut conn = Connection::open(URL).unwrap();
    conn.publish(b"order.placed", b"order-42").unwrap();
});

let ev = sub.recv()?;
// PubsubEvent::Message { channel: "order.placed", payload: "order-42" }
# Ok::<(), std::io::Error>(())
```

## Pattern 3 —— 环境驱动的 dev/prod 切换

同一份代码、三个后端:

```rust
use kevy_client::{Connection, Subscriber};

fn run_app(url: &str) -> std::io::Result<()> {
    let mut sub  = Subscriber::open(url, &[b"jobs"])?;
    let mut conn = Connection::open(url)?;
    let _ack = sub.recv()?;
    conn.publish(b"jobs", b"compute pi")?;
    // ... 排空 events ...
    Ok(())
}

// Dev:
run_app("mem://app")?;
// 带持久化的测试:
run_app("file:///tmp/app-test")?;
// Prod:
run_app("kevy://prod-cache:6379")?;
# Ok::<(), std::io::Error>(())
```

每个调用现场都没 `match scheme { ... }`。打开同一个 URL,两端附到同
一个底层总线。

## Pattern 4 —— glob 模式

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub = Subscriber::connect("mem://signals")?;
sub.psubscribe(&[b"sensor.*"])?;
let _ack = sub.recv()?;  // Psubscribe ack

let mut conn = Connection::open("mem://signals")?;
conn.publish(b"sensor.temp", b"22.5")?;  // 匹配
conn.publish(b"weather", b"sunny")?;     // 不匹配

if let PubsubEvent::Pmessage { pattern, channel, payload } = sub.recv()? {
    assert_eq!(pattern, b"sensor.*");
    assert_eq!(channel, b"sensor.temp");
    assert_eq!(payload, b"22.5");
}
# Ok::<(), std::io::Error>(())
```

Glob 语法:`*`(任意)、`?`(一字符)、`[abc]`(字符类)—— 跟 `KEYS` /
`SCAN` 同一 matcher。

## Pattern 5 —— 扇出到多个订阅者

```rust
use kevy_client::{Connection, Subscriber};

const URL: &str = "mem://fanout";
let mut s1 = Subscriber::open(URL, &[b"chan"])?;
let mut s2 = Subscriber::open(URL, &[b"chan"])?;
let _ = s1.recv()?;
let _ = s2.recv()?;

let mut conn = Connection::open(URL)?;
let received = conn.publish(b"chan", b"broadcast")?;
assert_eq!(received, 2);  // 两个都拿到了
# Ok::<(), std::io::Error>(())
```

## API 概要

```rust
// Producer
let mut conn = Connection::open(url)?;
let recv_count = conn.publish(channel, payload)?;

// Consumer
let mut sub = Subscriber::open(url, &[channel])?;          // open + subscribe
// 或
let mut sub = Subscriber::connect(url)?;                    // 先 open,后 subscribe
sub.subscribe(&[chan1, chan2])?;
sub.psubscribe(&[b"foo.*"])?;
sub.unsubscribe(&[chan1])?;       // 空 &[] → 退订所有 channel
sub.punsubscribe(&[])?;            // 空 &[] → 退订所有 pattern
sub.set_read_timeout(Some(Duration::from_secs(1)))?;
let ev: PubsubEvent = sub.recv()?;
```

`PubsubEvent` 有六个 variant:`Subscribe`、`Psubscribe`、
`Unsubscribe`、`Punsubscribe`、`Message`、`Pmessage`。`Unsubscribe` /
`Punsubscribe` 的 channel/pattern 槽用 `Option<Vec<u8>>` —— `None` 匹
配 "无 channel 被订阅" 的 nil-bulk wire 形状。

## 生命周期 + 陷阱

**进程内注册表**:URL → `Store` 映射是 per-process,`Weak` 引用支持。
当一个 named URL 的最后一个 `Connection` / `Subscriber` drop,entry 释
放;下次 open 同 URL 得新的 `Store`。(`file:///` URL 时磁盘 AOF + snapshot 保留;重新 open 时 replay。)

**跨进程**:`mem://name` 和 `file:///path` **不**对其他进程可见。要
真跨进程递送,起一个 kevy server 用 `kevy://host:port`。

**Ack 顺序**:`SUBSCRIBE` 把 `Subscribe` ack 在该 channel 的任何
`Message` 之前 enqueue 到 receive queue。测试里在断言消息体之前排空
ack。

**发送时序**:bus mutex 在 `Sender::send()` 调用前释放,所以慢
receiver 不会拖延其他不相干 channel 的 publish。每个订阅者有自己的
`mpsc::Receiver` 队列(无共享 bound)。

**`Subscription` drop 时原子注销**:线程 panic 不会留 "陈旧订阅者"
zombie 状态 —— `Drop` impl 遍历 bus 表移除每个标了订阅 id 的 entry。

**匿名 `mem://` 上的 `Connection::publish`** 永远返 0(没有可能的订
阅者)。`mem://<name>` 上返回真实接收方数。

**TLS / AUTH** 不支持。需要的话在网络边界用 stunnel + IP 白名单。

## 异步 runtime(tokio / async-std / smol)

`Subscription` 和 `Subscriber` 是 `Send + Sync` —— `Arc<Subscription>`
能用,多个 async task(或 `spawn_blocking` job)能共享一个 handle。
blocking `recv` API 故意保留:kevy 出货零 crates.io 依赖,async-runtime
不可知的 future 得手写。三个干净 pattern:

**Pattern A —— 专用 OS 线程 + runtime channel**(单 consumer,无需共
享 handle):

```rust,no_run
# use kevy_embedded::{Config, PubsubFrame, Store};
# let store = Store::open(Config::default().with_ttl_reaper_manual())?;
// 伪代码 —— 把 `runtime_channel` 换成 tokio::sync::mpsc /
// async_channel / 等按你 runtime 决定。
let (tx, rx) = /* runtime_channel */;
std::thread::spawn({
    let store = store.clone();
    move || {
        let sub = store.subscribe(&[b"queue:notify"]);
        while let Ok(frame) = sub.recv() {
            if matches!(
                frame,
                PubsubFrame::Message { .. } | PubsubFrame::Pmessage { .. }
            ) && tx.blocking_send(()).is_err()
            {
                break; // receiver dropped
            }
        }
    }
});
// `rx` 是 async 侧 handle;在你 async loop 里 await。
# Ok::<(), std::io::Error>(())
```

这是 mailrs 的出站队列 worker 用的方式 —— 小、长寿命的 task;避免每次
recv 占一个 tokio blocking-pool 槽位。

**Pattern B —— `Arc<Subscription>` + `spawn_blocking`**(多个 async
task 共享一个 handle):

```rust,no_run
# use kevy_embedded::{Config, Store};
# use std::sync::Arc;
# let store = Store::open(Config::default().with_ttl_reaper_manual())?;
let sub = Arc::new(store.subscribe(&[b"queue:notify"]));
// 每个 async task 拿自己 Arc clone,通过 spawn_blocking recv;
// receiver mutex 把并发 recv 串行化。每帧只递给一个 task(**不**广播)。
//
// 要广播扇出(每个 consumer 看每条消息),给每个 consumer 开独立
// Subscription —— 它们便宜。
let task_handle = {
    let sub = sub.clone();
    // tokio::task::spawn_blocking 伪:
    std::thread::spawn(move || {
        loop {
            match sub.recv() {
                Ok(frame) => { /* 处理 */ let _ = frame; }
                Err(_) => break, // bus 关闭
            }
        }
    })
};
# let _ = task_handle;
# Ok::<(), std::io::Error>(())
```

`Subscription::try_recv` 用 `try_lock`,锁竞争下返 `Ok(None)` —— 即
使另一个 task 通过 `recv` 持有 receiver 时,non-blocking 契约也保留。

**Pattern C —— `kevy-client::Subscriber` 上的借用迭代器**:

```rust,no_run
# use kevy_client::Subscriber;
let mut sub = Subscriber::open("mem://news", &[b"updates"])?;

// `events()` yield 每一帧(含 ack)。UnexpectedEof 时结束;其他错误
// 以 Some(Err(_)) 形式露出,caller 决定 retry(例如 read timeout)还
// 是 break。
for event in sub.events() {
    let _ = event?; // dispatch
    # break;
}

// `messages()` 静默吃掉 ack,只 yield `(channel, payload)` —— 跟
// recv_message 同形。
let mut sub2 = Subscriber::open("mem://news", &[b"updates"])?;
for msg in sub2.messages() {
    let (_channel, _payload) = msg?;
    # break;
}
# Ok::<(), std::io::Error>(())
```

同样 `spawn_blocking` 规则适用:迭代器包 `recv` / `recv_message`,
在每次 blocking wait 期间拿 receiver mutex。drop 或 break 迭代器以
提前释放 wait。迭代器 API 属于 `kevy-client`,不是 `kevy-embedded`
—— 如果你想要它,对着 URL facade 开 `Subscriber`;要 embed-only
原语就直接抓 `Subscription`。

## 相关

- [`kevy-embedded` 1.2.0+](https://crates.io/crates/kevy-embedded) ——
  底层 `Store::Clone` + `PubsubBus` 原语。如果你不需要 URL facade 间
  接层,直接用它。
- [`kevy-client` 1.9.0+](https://crates.io/crates/kevy-client) ——
  URL facade 本身。ships `Subscriber::recv_message`、`events()` /
  `messages()` 迭代器、给 slot 路由 keyspace 流量的 `ClusterClient`
  (pub/sub 不需要 —— 见上面集群说明)。
- [`kevy`](https://crates.io/crates/kevy) —— TCP 服务器(1.17.0+),
  单进程长不大时用。
- [`docs/cluster.md`](cluster.md) —— 集群模式 + 给 slot 路由 keyspace
  流量的 `ClusterClient`。
