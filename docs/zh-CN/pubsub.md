# Pub/sub

本文讲 kevy 里发布者如何把消息扇出给多个订阅者——既可以走网络用 `PUBLISH` / `SUBSCRIBE`，也可以通过嵌入式 `Store` 在进程内完成，还可以走 `kevy-client` 其余部分共用的那套 URL 门面。

## 何时需要

当一个写入方需要*马上*通知零个或多个读取方，而且不在乎读取方离线期间发布的消息时，pub/sub 就是合适的工具：

- “通知每个 web worker 刷新自己的配置缓存。”
- “把某个 shard 刚写入的行，流式推给所有正在跟读的人。”
- “任务一到就唤醒 worker 池；任务本身放在 list 里。”
- “开发场景：生产者线程和消费者线程跑在同一个二进制里，不用另起一台 Redis。”

如果交接必须耐久（带重试的任务队列、跨重启的扇出、消息重放），请改用 list 或 stream——哪些内容会落盘，见 [`docs/persistence.md`](persistence.md)。

## 核心思路

pub/sub 频道就是一个名字。订阅者在这个名字（或一个 glob 模式）上登记兴趣；向同名频道发布时，系统遍历订阅者索引，给每个匹配的订阅者入队一份消息体。这里没有 broker 队列、没有离线缓冲、没有 ack——发布那一刻没人在听，消息就没了。

```
                   publish("news", body)
                          |
                          v
             +-----------------------+
             |  channel "news"       |   <- per-channel subscriber index
             |  subscribers: [A,B,C] |
             +-----------------------+
                  |       |       |
                  v       v       v
               sub A   sub B   sub C    <- each gets its own copy
```

内部实现上，每次 publish 只构建一次协议帧，把消息体包进 `Arc`，再用 `writev` 分散写给每个匹配的 TCP 订阅者——所以无论扇出多宽，消息体字节的额外拷贝次数都是**零**。同一份每频道索引同时服务着服务器连接和进程内的 `Subscription` 句柄。

## 实际示例

### 用 `redis-cli` 冒烟测试

对着一台运行中的 kevy 服务器开两个 shell：

```sh
# shell 1 — subscriber
$ redis-cli -p 6379 SUBSCRIBE news
Reading messages... (press Ctrl-C to quit)
1) "subscribe"
2) "news"
3) (integer) 1
```

```sh
# shell 2 — publisher
$ redis-cli -p 6379 PUBLISH news "hello"
(integer) 1   # one subscriber received it
```

回到 shell 1：

```
1) "message"
2) "news"
3) "hello"
```

对没有订阅者的频道执行 `PUBLISH`，返回 `(integer) 0`，消息直接丢弃。契约就是这样——不会有“我们尝试投递过”之类的信号。

### Rust 走 URL 门面——`kevy-client`

同一套调用形态，既能打 TCP 服务器，也能打命名的进程内总线，还能打持久化的进程内 store；换个 URL 重新编译就行，调用点不需要 `match scheme { … }`。

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

fn run(url: &str) -> std::io::Result<()> {
    // Open a subscriber against `news`. The first frame the bus
    // hands back is the subscribe ack; drain it before asserting
    // on bodies.
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

// Dev:  in-process shared bus by name.
run("mem://app")?;
// Prod: real TCP server.
run("kevy://prod-cache:6379")?;
# Ok::<(), std::io::Error>(())
```

跨线程用法就是同一份代码：在不同线程里对同一个 URL 各开一个 `Subscriber` 和一个 `Connection`——`mem://<name>` 注册表会把同一条底层总线交给两端，生产者线程调 `Connection::publish`，消费者线程阻塞在 `sub.recv()` 上。

### 进程内直连：`kevy-embedded`

嵌入方代码手里已经有 `Store` 时，可以跳过 URL 这层间接，直接跟总线打交道：

```rust
use kevy_embedded::{Config, PubsubFrame, Store};

let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// Subscriber owns the receive queue.
let sub = store.subscribe(&[b"jobs"]);
let _ack = sub.recv()?; // PubsubFrame::Subscribe

// Any clone of `store` reaches the same bus.
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

`Store::clone` 很便宜（只是 `Arc` 引用计数加一），所以常见写法是给每个线程发一份 `store.clone()`，需要时各自 `publish` 或 `subscribe`。订阅者 drop 时会原子地注销；消费者线程即使 panic，也不会在索引里留下僵尸条目。

### 模式订阅

`PSUBSCRIBE` 注册一个 glob，凡是匹配它的频道上的消息都会收到。glob 语法——`*`、`?`、`[abc]`——和 `KEYS`、`SCAN` 用的是同一个匹配器。

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub = Subscriber::connect("mem://signals")?;
sub.psubscribe(&[b"news.*"])?;
let _ack = sub.recv()?;            // PubsubEvent::Psubscribe

let mut conn = Connection::open("mem://signals")?;
conn.publish(b"news.tech", b"breaking")?; // matches
conn.publish(b"weather",   b"sunny")?;    // does NOT match

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

同一个订阅者如果既订阅了某频道，**又**订阅了能匹配该频道的模式，就会收到**两份**消息——一份 `Message`，一份 `Pmessage`。发布时的去重只压掉“同一个 `Subscription` 在同一频道索引里出现两次”这种重复，不会去掉频道订阅与模式订阅的重叠。

## URL 后端表

| URL                                | 后端 store                 | 多次 open 是否共享？                              | 是否跨进程可见？ |
|------------------------------------|----------------------------|---------------------------------------------------|-----------------|
| `mem://`                           | 进程内，匿名               | **否**——每次 open 都是全新的 `Store`              | 否              |
| `mem://<name>`                     | 进程内，命名注册表         | **是**——同一 `<name>` ⇒ 同一 `Store`              | 否              |
| `file:///abs/path`                 | 进程内 + AOF/快照          | **是**——同一路径 ⇒ 同一 `Store`，且持久化          | 否              |
| `kevy://host[:port][/db]`          | TCP kevy 服务器            | 每次 open 一条 socket；扇出在服务端完成            | **是**          |
| `redis://host[:port][/db]`         | TCP——`kevy://` 的别名      | 同上                                              | **是**          |
| `tcp://host[:port]`                | TCP——原始连接，不带前置 `SELECT` | 同上                                        | **是**          |

匿名 `mem://` 收不到发布的消息——别的代码根本够不到同一个底层 `Store`，所以 `Subscriber::open` 会以 `ErrorKind::Unsupported` 拒绝它。只要打算发布，就用 `mem://<some-name>`。

`rediss://`、`kevys://` 和 `redis://user:pass@…` 被拒绝也是同一个原因：kevy 不带 TLS，也不带 `AUTH`。需要这两样时，在网络边界用 stunnel 加 IP allowlist 把 socket 挡在前面。

`mem://<name>` 和 `file:///` 的注册表都是**进程级**的：两个互不相干的 OS 进程打开同一个名字，看到的是两条彼此独立的总线。要跨进程投递，就得跑一台 kevy 服务器，两边都连 `kevy://host:port`。

## 取舍与限制

- **至多一次投递。** 订阅者在一帧投递中途断线，那一帧就丢了。既没有每订阅者的耐久游标，也没有重投。某一帧要是丢不起，就把它持久化到 list 或 stream 里，pub/sub 只当“叫醒”信号用。
- **没有离线 backlog。** 发布时一个订阅者都没有，调用返回 `0`，消息体直接丢弃。没有任何缓冲区能帮订阅者补上断线期间错过的消息。
- **订阅者反压是按订阅者算的，不是全局的。** 每个订阅者各有一条有界队列。慢消费者填满的是自己的队列，之后开始丢帧；TCP 场景下则会被服务器的客户端输出缓冲策略断开。发布路径在发送之前就释放了总线锁，所以一个慢的监听者拖不慢其他频道上的发布——但反过来，它也没法对发布者施加反压。
- **Linux `writev` 上限。** Linux 上一次 `writev` 最多向内核递交 `IOV_MAX = 1024` 个 iovec。服务器会把每订阅者的帧头和共享的消息体 Arc 打包成 iovec；单频道扇出超过约 340 个订阅者时（每个订阅者占三个 iovec 槽），服务器会自动拆成多次 `writev`。这个上限只会表现为软性的性能天花板，绝不会造成投递失败。
- **订阅中的客户端受限。** `Subscriber` 连接会拒绝非 pub/sub 命令；这正是 `kevy-client` 把发布者和订阅者拆成**两个独立类型**、共用同一个 URL 的原因。

## 运维内省

标准的 `PUBSUB` 管理子命令在 TCP 服务器和 URL 门面上都能用——调用时开一条普通 `Connection`，不要用 `Subscriber`。

| 子命令                  | 返回                                                                              |
|-------------------------|-----------------------------------------------------------------------------------|
| `PUBSUB CHANNELS [pat]` | 至少有一个订阅者的频道数组，可选 glob 过滤。                                       |
| `PUBSUB NUMSUB [ch …]`  | 每个指定频道的 `channel, count` 交错对（频道不存在时计数为 0）。                    |
| `PUBSUB NUMPAT`         | 整数：全体客户端注册的不同 `PSUBSCRIBE` 模式总数。                                  |

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

三个子命令都只是对每 shard 的 pub/sub 注册表做 O(channels) 或 O(args) 的点查；监控代理放心轮询。

## FAQ

**订阅者在发布之后才连上，还能收到那条消息吗？** 收不到。pub/sub 没有回放：订阅者索引在发布那一刻查询，后来的订阅者只能看到自己的订阅 ack 落地*之后*发布的帧。

**`PUBLISH` 会阻塞发布者，直到订阅者把消息消费完吗？** 不会。消息体入队到每个匹配订阅者各自的队列（TCP 订阅者还会排进 socket 的写队列）之后，`publish` 调用就返回了。慢订阅者堵的是它自己的队列，堵不到你。

**能在多个 async 任务之间共享一个 `Subscriber` 吗？** 能——用 `Arc` 包起来，把 `recv` 调用放进 `spawn_blocking`。接收侧的互斥锁会把阻塞等待串行化，所以每一帧**恰好**交给一个任务。想要真正的广播扇出（每个任务都看到每一帧），就给每个任务各开一个 `Subscriber`——开销很小。完整的 async 模式见 [`docs/async.md`](async.md)。

**为什么我的测试总是先收到 subscribe ack，才收到消息？** 总线是有序的，而每次 `SUBSCRIBE` / `PSUBSCRIBE` 都会先入队一帧 ack，该频道的第一帧消息体在它之后才会到。先用一次 `sub.recv()?` 把 ack 取掉，再对载荷做断言——这和 redis-cli 的线上行为一致。

**pub/sub 需要集群路由吗？** 不需要。pub/sub 的扇出在进程层面完成，不走 slot 路由：向任意 shard 端口发布，同一进程内所有 shard 端口上的订阅者都能收到。随便挑一个 shard 端口做普通的 `Connection::open("kevy://host:port")` 就行。*键空间*命令用的 slot 路由见 [`docs/cluster.md`](cluster.md)。
