# Pub/sub

kevy 中发布者如何把消息扇出给众多订阅者 —— 通过 `PUBLISH` / `SUBSCRIBE` 走线协议、通过嵌入式 `Store` 在进程内,以及通过与其余 `kevy-client` 共用的 URL 门面。

## 何时需要

当一个写者需要*现在*通知零或多个读者,且你不关心读者离线时到达的消息,就请使用 pub/sub:

- "通知每个 web worker 刷新自己的配置缓存。"
- "把刚写入的某 shard 的行流给任何在监听的人。"
- "有任务到来时唤醒 worker 池;任务本身放在 list 里。"
- "开发循环:同一个二进制里的生产者线程与消费者线程,不需要 Redis 实例。"

如果你需要可靠交接(带重试的任务队列、跨重启的扇出、消息重放),请改用 list 或 stream —— 见 [`docs/persistence.md`](persistence.md) 了解什么会写到磁盘。

## 核心思路

pub/sub 频道是一个名字。订阅者在该名字(或一个 glob 模式)上注册兴趣;对同名字的发布会遍历订阅者索引,并把消息体复制一份入每个匹配订阅者的队列。没有 broker 队列,没有离线缓冲,没有 ack —— 你发布的瞬间没人在听,这条消息就没了。

```
                   publish("news", body)
                          |
                          v
             +-----------------------+
             |  channel "news"       |   <- 每频道订阅者索引
             |  subscribers: [A,B,C] |
             +-----------------------+
                  |       |       |
                  v       v       v
               sub A   sub B   sub C    <- 每人各拿一份
```

内部上每次 publish 都只构建一次线协议帧,把消息体包成 `Arc`,然后用 `writev` 把它分散到每个匹配的 TCP 订阅者 —— 因此无论扇出有多宽,消息体字节都**零**额外拷贝。同一份每频道索引同时处理服务器连接和进程内 `Subscription` 句柄。

## 实际示例

### 用 `redis-cli` 烟测

对一台运行中的 kevy 服务器开两个 shell:

```sh
# shell 1 — 订阅者
$ redis-cli -p 6379 SUBSCRIBE news
Reading messages... (press Ctrl-C to quit)
1) "subscribe"
2) "news"
3) (integer) 1
```

```sh
# shell 2 — 发布者
$ redis-cli -p 6379 PUBLISH news "hello"
(integer) 1   # 一个订阅者收到
```

回到 shell 1:

```
1) "message"
2) "news"
3) "hello"
```

对没有订阅者的频道 `PUBLISH` 会返回 `(integer) 0`,消息被丢到地上。这就是契约 —— 你不会得到一个"我们尝试过投递它"的信号。

### Rust 走 URL 门面 —— `kevy-client`

同一种调用形态可以打 TCP 服务器、命名的进程内总线,或者持久的进程内 store;只换 URL 重编即可,调用点不必 `match scheme { … }`。

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

fn run(url: &str) -> std::io::Result<()> {
    // 在 `news` 上开一个订阅者。总线交回的第一帧是
    // subscribe ack;断言消息体之前先把它取掉。
    let mut sub = Subscriber::open(url, &[b"news"])?;
    let _ack = sub.recv()?;

    let mut conn = Connection::open(url)?;
    let received = conn.publish(b"news", b"hello")?;
    assert_eq!(received, 1);

    match sub.recv()? {
        PubsubEvent::Message { channel, payload } => {
            assert_eq!(channel, b"news");
            assert_eq!(payload, b"hello");
        }
        other => panic!("unexpected frame: {other:?}"),
    }
    Ok(())
}

// 开发:命名的进程内共享总线。
run("mem://app")?;
// 生产:真 TCP 服务器。
run("kevy://prod-cache:6379")?;
# Ok::<(), std::io::Error>(())
```

跨线程也一样:一个 `Subscriber` 与一个 `Connection` 在不同线程中对同一个 URL 打开 —— `mem://<name>` 注册表把两端交回同一个底层总线,所以生产者线程可以 `Connection::publish`,消费者线程阻塞在 `sub.recv()` 上。

### 通过 `kevy-embedded` 在进程内

嵌入代码已经有一个 `Store` 时,跳过 URL 间接,直接与总线对话:

```rust
use kevy_embedded::{Config, PubsubFrame, Store};

let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// 订阅者拥有接收队列。
let sub = store.subscribe(&[b"jobs"]);
let _ack = sub.recv()?; // PubsubFrame::Subscribe

// `store` 的任意 clone 都达到同一个总线。
let writer = store.clone();
assert_eq!(writer.publish(b"jobs", b"compute-pi"), 1);

match sub.recv()? {
    PubsubFrame::Message { channel, payload } => {
        assert_eq!(channel, b"jobs");
        assert_eq!(payload, b"compute-pi");
    }
    other => panic!("unexpected frame: {other:?}"),
}
# Ok::<(), std::io::Error>(())
```

`Store::clone` 很便宜(就是 `Arc` 自增),所以常见形态是"给每个线程一个 `store.clone()`,谁要 `publish` 或 `subscribe` 自己 publish 或 subscribe"。订阅者 drop 原子地取消注册;一个 panic 的消费者线程不会在索引里留下僵尸条目。

### 模式订阅

`PSUBSCRIBE` 注册一个 glob,在每个匹配它的频道上接收消息。glob 语法 —— `*`、`?`、`[abc]` —— 与 `KEYS` 和 `SCAN` 使用的匹配器一致。

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub = Subscriber::connect("mem://signals")?;
sub.psubscribe(&[b"news.*"])?;
let _ack = sub.recv()?;            // PubsubEvent::Psubscribe

let mut conn = Connection::open("mem://signals")?;
conn.publish(b"news.tech", b"breaking")?; // 匹配
conn.publish(b"weather",   b"sunny")?;    // 不匹配

match sub.recv()? {
    PubsubEvent::Pmessage { pattern, channel, payload } => {
        assert_eq!(pattern, b"news.*");
        assert_eq!(channel, b"news.tech");
        assert_eq!(payload, b"breaking");
    }
    other => panic!("unexpected frame: {other:?}"),
}
# Ok::<(), std::io::Error>(())
```

同时持有一个频道订阅**与**一个匹配模式订阅的订阅者会收到**两份**拷贝 —— 一份 `Message`、一份 `Pmessage`。每发布去重只压制"同一 `Subscription` 在同一频道索引里出现两次"的重复,不压制 channel 与 pattern 的重叠。

## URL 后端表

| URL                                | 后端 store                 | 多次 open 是否共享?                              | 是否跨进程可见? |
|------------------------------------|----------------------------|---------------------------------------------------|-----------------|
| `mem://`                           | 进程内,匿名               | **否** —— 每次 open 得到全新 `Store`              | 否              |
| `mem://<name>`                     | 进程内,命名注册表         | **是** —— 同 `<name>` ⇒ 同 `Store`                | 否              |
| `file:///abs/path`                 | 进程内 + AOF / 快照        | **是** —— 同路径 ⇒ 同 `Store`,持久                | 否              |
| `kevy://host[:port][/db]`          | TCP kevy 服务器            | 每次 open 一个 socket;服务端扇出                  | **是**          |
| `redis://host[:port][/db]`         | TCP —— `kevy://` 的别名    | 同上                                              | **是**          |
| `tcp://host[:port]`                | TCP —— 原始,无前置 `SELECT` | 同上                                            | **是**          |

匿名 `mem://` 无法接收已发布消息 —— 没有其它东西能到达同一个底层 `Store`,所以 `Subscriber::open` 以 `ErrorKind::Unsupported` 拒绝它。打算发布时请用 `mem://<some-name>`。

`rediss://`、`kevys://`、`redis://user:pass@…` 出于同样原因被拒绝:kevy 出厂不含 TLS 或 `AUTH`。需要其中之一时,请在网络边界用 stunnel + IP allowlist 把 socket 包起来。

`mem://<name>` 与 `file:///` 注册表是**每进程**的:两个不相关的 OS 进程打开同一个名字会看到两条彼此独立的总线。跨进程投递意味着跑一台 kevy 服务器并从双方都打开 `kevy://host:port`。

## 取舍与限制

- **至多一次投递。** 一个在帧投递中途断开的订阅者会丢掉那一帧。没有每订阅者的耐久游标,也没有重投。如果某帧重要,把它持久化进 list 或 stream,把 pub/sub 只当作"叫醒"信号。
- **没有离线 backlog。** 一次发现零订阅者的发布返回 `0`,消息体被丢弃。没有"补帧"的缓冲。
- **订阅者反压是每订阅者的,不是全局的。** 每个订阅者拥有自己有界的队列。慢消费者把自己的队列填满后就丢帧,或者在 TCP 上被服务器的客户端输出缓冲策略关闭。发布路径在发送前会先 drop 总线锁,所以一个慢监听者不能阻塞与它无关的频道上的发布 —— 但它也无法对发布者施加反压。
- **Linux `writev` 上限。** Linux 上 `writev` 一次最多向内核交 `IOV_MAX = 1024` 个 iovec 项。服务器把每订阅者帧头与共享消息体 Arc 批入 iovec;每个 channel 扇出 ~340 个以上订阅者时(每个占用三个 iovec 槽)服务器会自动拆成多次 `writev`。该上限只表现为一个软的性能上限,绝不会表现为投递失败。
- **被订阅的客户端受限。** `Subscriber` 连接会拒绝非 pub/sub 命令;这就是为什么 `kevy-client` 把发布者和订阅者作为**两个独立类型**暴露,但共用同一个 URL。

## 运维内省

标准 `PUBSUB` 管理子命令在 TCP 服务器与 URL 门面上都可用 —— 调用它们时请打开普通的 `Connection`,不是 `Subscriber`。

| 子命令                  | 返回                                                                              |
|-------------------------|-----------------------------------------------------------------------------------|
| `PUBSUB CHANNELS [pat]` | 至少有一个订阅者的频道数组,可选 glob 过滤。                                       |
| `PUBSUB NUMSUB [ch …]`  | 每个命名频道交错的 `channel, count` 对(若不存在则为 0)。                          |
| `PUBSUB NUMPAT`         | 整数:所有客户端中已注册的不同 `PSUBSCRIBE` 模式数。                                |

```sh
$ redis-cli -p 6379 PUBSUB CHANNELS '*'
1) "news"
2) "jobs"
$ redis-cli -p 6379 PUBSUB NUMSUB news jobs missing
1) "news"
2) (integer) 3
3) "jobs"
4) (integer) 1
5) "missing"
6) (integer) 0
$ redis-cli -p 6379 PUBSUB NUMPAT
(integer) 2
```

三个都是对每 shard pub/sub 注册表的 O(channels) 或 O(args) 点查询;供监控代理轮询是安全的。

## FAQ

**订阅者在发布之后才连上,还能收到那条消息吗?**  不能。pub/sub 没有回放。订阅者索引在发布时被查询;之后才订阅的人只看到自己订阅 ack 之后发布的帧。

**`PUBLISH` 会阻塞发布者直到订阅者把队列消耗完吗?**  不会。发布者的 `publish` 调用在消息体被入队到每个匹配订阅者的队列后就返回(对 TCP 订阅者,还会被调度进 socket 的写队列)。慢订阅者只阻塞它自己的队列,不阻塞你。

**我能在多个 async 任务间共享一个 `Subscriber` 吗?**  可以 —— 用 `Arc` 包,把 `recv` 调用 `spawn_blocking`。接收互斥锁串行化阻塞等待,因此每帧**恰好**投递给一个任务。要真广播扇出(每个任务都看到每一帧),每个任务各开一个 `Subscriber` —— 它们便宜。完整 async 模式见 [`docs/async.md`](async.md)。

**为什么我的测试在任何消息之前看到了 subscribe ack?**  总线有序,但每次 `SUBSCRIBE` / `PSUBSCRIBE` 都会在该频道的第一条消息体之前入队一帧 ack。先用一次 `sub.recv()?` 把 ack 取掉,再断言载荷 —— 这与 redis-cli 线协议形态一致。

**pub/sub 需要集群路由吗?**  不需要。pub/sub 扇出是进程层的,不是 slot 路由的:在任一 shard 端口上发布,会到达同一进程内所有 shard 端口的每个订阅者。对任一 shard 端口做朴素的 `Connection::open("kevy://host:port")` 即可。键空间命令所使用的 slot 路由见 [`docs/cluster.md`](cluster.md)。
