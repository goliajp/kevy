# Embedded 只读 RESP listener

一个 embedded（进程内）的 kevy store，可以通过一个只读 listener 把自己暴露给外部 RESP 客户端——redis-cli、运维工具、看板。store 依然是你进程里的一个库；listener 是望进去的一扇窗，不是第二台服务器：写入仍然是持有进程的专属权力。

```rust
use kevy_embedded::{Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(
        Config::default()
            .with_shards(4)
            .with_resp_listener("127.0.0.1:6009".parse().unwrap()),
    )?;
    store.hset(b"row:42", &[
        (b"state".as_slice(), b"live".as_slice()),
    ])?;
    // ... the application keeps running; clients can peek:
    Ok(())
}
```

```
$ redis-cli -p 6009 hgetall row:42
1) "state"
2) "live"
$ redis-cli -p 6009 scan 0 match 'row:*' count 100
$ kevy-cli -p 6009 DBSIZE
```

任何 RESP 客户端都能用——listener 讲的协议和 kevy 服务器一样，既接受成帧的请求，也接受 inline 命令（`redis-cli` 那种 PING 风格）。

## 打开它

- `Config::with_resp_listener(addr)`——一个 `SocketAddr`。**默认关闭**；关闭时**没有线程、也没有 socket——零税**（有 gate：listener 开着但空闲时，写吞吐与关闭时相差在 10% 以内，见 [`bench/topogate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/topogate.sh)）。
- 代码在 `listener` 这个 cargo feature 后面（默认开启；wasm32 目标上不可用）。
- listener 只持有一个弱句柄：它永远不会把 store 续命，store 一旦 drop，连接随之结束。
- 没有认证（这是刻意的：kevy 没有 AUTH 平面）。绑回环或私网接口；任何能连上来的人，都能读到白名单所服务的一切。

## 接口面

只有白名单——其余一律回答 `-ERR READONLY embedded listener`：

```
PING ECHO GET MGET EXISTS TYPE TTL PTTL DBSIZE KEYS SCAN
HGET HMGET HGETALL HLEN LRANGE LLEN SMEMBERS SCARD SISMEMBER
ZSCORE ZCARD ZRANGE FEED.READ FEED.TAIL FEED.SHARDS INFO
```

被挡在外面的包括每一个写 verb、`MULTI`、阻塞 pop、pub/sub，以及各个扩展平面（`IDX.*` / `VIEW.*`——那些请在进程内通过类型化 API 查询）。`FEED.*` 这三个需要 `replicate` cargo feature（默认开启）。

`INFO` 回答一份 embedded 形态的小报告——crate 版本、shard 数、键数，以及 `listener:readonly`——足够让看板认出自己在跟什么说话。

那句拒绝文本是一份契约：工具可以靠“试写一次、匹配 `READONLY embedded listener`”来探测“这是一台真服务器，还是一扇 embedded 的窗”。

### Verb 语义

白名单里的 verb，行为与它们在服务器上的同名兄弟一致：

- `SCAN <cursor> [MATCH pattern] [COUNT n]` 以数字游标分页（`COUNT` 夹到 1..=10000，默认 100），并给出通常的 SCAN 保证：整趟遍历期间稳定存在的键恰好被看到一次；并发的插入 / 删除可能出现，也可能不出现。
- `KEYS <pattern>` 一次回复里走完整个键空间（`*` 匹配一切）——只适合抽查，大 store 上请用 `SCAN`。
- 类型不匹配回答 `-WRONGTYPE …`，与服务器一模一样；整数格式非法回答 `-ERR value is not an integer`。

## 一致性

读取跑在 store 自己的 shard 锁下——每一条回复都是一个已提交的时间点答案，与正在写的那个进程实时同源（没有复制、没有滞后、没有快照陈旧）。多键读取（`MGET`、`SCAN`、`KEYS`、`DBSIZE`）逐 shard 归并，没有全局快照——与服务器同属 SCAN 类信封。

每条连接一个线程：这是一个运维工具接口面，不是服务路径（kevy **服务器**才是服务路径）。把连接数控制在工具的量级上；一个每几秒轮询一次的看板，就是它预期的形状。

## Feed 与读己之写

`FEED.TAIL` 返回当前的 `(generation, offset)`；`FEED.READ <gen> <offset> <limit> [PREFIX p…]` 投递变更帧——契约与 embedded 的 `changes_since` API 一样是 at-least-once（陈旧的 generation 回答 `Resync`，从 `FEED.TAIL` 重启）。embedded 的写路径把所有 shard 串成一条流，所以 `FEED.SHARDS` 恒报 1，feed 三个 verb 不带 shard 参数——照着服务器接口面写的那份消费者循环（[cdc.md](cdc.md)）在这里照样跑。

跨进程的读己之写，在 feed 上是一个**游标模式**，不是阻塞原语：写进程在写完后记下 `changes_tail()`；读进程先把 `FEED.READ` drain 过那个游标，再去读。进程内的读永远是读己之写（写入是同步提交的）。（服务器到副本的复制**确实**有阻塞原语——`REPL.TOKEN` / `REPL.WAIT`，见 [availability.md](availability.md)——但那是复制平面，不是这个 feed listener。）

## 不开 socket 也能内省

listener 的 verb 表同时也是一个公开方法：

```rust
let mut out = Vec::new();
store.dispatch_readonly(
    &[b"HGETALL".to_vec(), b"row:42".to_vec()], &mut out);
// `out` holds raw RESP bytes — the exact reply the listener
// would have written to a socket.
```

`Store::dispatch_readonly(argv, out)` 对着同一份白名单回答一次请求（写 verb 拿到同样的 `-ERR`）——这是 listener 的编程接口面，给嵌在持有进程内部的工具用。

至于一个**不归你所有**的 store：`kevy-cli --embed <dir>` 打开该 embedded store 数据目录的一个只读时间点视图——dump / aof / shards.meta 这些文件会被**复制**到一个临时目录再重放，所以持有它的进程照跑不误、毫发无损。REPL 和一次性调用都能用；写 verb 回答 listener 那句 `-ERR READONLY`。那是这扇实时窗口的离线补集。

## 当一扇窗不够用时

按机械复杂度排序的升级路径：

1. **这个 listener**——实时读，零写入成本，单进程。
2. **CDC feed**（[cdc.md](cdc.md)）——把变更推给另一个进程，而不是让它来轮询读。
3. **Embedded 作为主节点的复制**——`with_embed_writer` 暴露一个复制源，配了 `[replication] single_source = true` 的 kevy 服务器以副本身份跟随这个 embedded store：读扇出跑在服务器硬件上，喂料的是你的进程。见 [replication.md](replication.md) 的 *Embedded 作为主节点* 一节。

## 性能

[`bench/topogate.sh`](https://github.com/goliajp/kevy/blob/develop/bench/topogate.sh) 是那道钳制，而且它是一个**真正的双进程**测试——一个写进程二进制持续跑 HSET 负载，另一个独立的读进程断言数据是实时的：

- 在持有者持续写入的同时，读进程 `GET` p99 < 1ms（6 条连接的中位数）。
- READONLY 强制生效（写 verb 被那句契约文本拒绝）。
- 零税：listener 开着但空闲时，持有者的写吞吐与 listener 关闭时相差在 10% 以内。

“读跑在 shard 锁下”这个设计意味着：大流量的 listener 读取**确实会**和持有者的写入在同一批 shard 上争锁——这是换取零滞后真值所付的代价。工具量级的读速率（看板、抽查）在持有者的写吞吐里量不出来；如果你需要的是服务量级的读，请用复制（见上），而不是这个 listener。

## 另见

- [cdc.md](cdc.md)——这个 listener 同样服务的变更流。
- [replication.md](replication.md)——embedded 作为主节点，当一扇窗不再够用时。
- [availability.md](availability.md)——复制平面上的一致性阶梯。
- [uds.md](uds.md)——kevy 服务器的本地 socket 传输（listener 本身只讲 TCP）。
