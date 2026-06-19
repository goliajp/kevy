# 持久化

kevy 怎么把数据保留过重启:AOF、二进制 snapshot、TTL 语义、AOF 重写/
压缩、崩溃恢复,以及看这一切的 introspection。同时适用于网络服务器
(`kevy` 二进制)和进程内嵌入式模式(`kevy_embedded::Store`);差异会
明确指出。

## 两个磁盘 artifact

持久化在一个目录里(服务器:`--dir` / `KEVY_DIR`;嵌入式:
`Config::with_persist(dir)`)。每个 shard 拥有自己的文件,以 shard id
作后缀:

| 文件 | 内容 | 由谁写 |
|------|------|------|
| `aof-<id>.aof` | 每条变更命令的 append-only 日志(RESP 帧,前缀 `KEVYAOF1` magic) | 持续写入 |
| `dump-<id>.rdb` | 二进制 point-in-time snapshot(magic `KEVYSNAP`) | 仅在显式 `SAVE` / `BGSAVE`(服务器)或 `Store::save_snapshot`(嵌入式)时 |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | 恢复时被搬开的损坏 AOF 尾 | 启动时,如果 AOF 尾无法解析 |

默认配置下,新建的嵌入式 store **仅靠 AOF** 持久化 —— 没有 snapshot
文件出现,除非你调 `save_snapshot`。这是预期行为,不是缺失功能:AOF
本身就是完整的持久记录。

## Snapshot:`SAVE`、`BGSAVE`、`Store::save_snapshot`

一个成功的 snapshot 会**重置 AOF**:snapshot 携带 snapshot 点的状态,
日志重启,只记录在它之后落地的写操作 —— 这样重启时加载 snapshot + 日
志而永不双重应用历史(v1.16.0 也修了嵌入式 `save_snapshot`,之前会留
完整 log 不动,replay 时让像 `RPUSH` 这类非幂等命令重复)。

- **`SAVE`**(服务器)是同步阻塞-持久化,符合 Redis 契约:在 snapshot
  落到磁盘后才返回。如果某 shard 已有后台 save/rewrite 在跑,该 shard
  的 `SAVE` 会带 log 行跳过。
- **`BGSAVE`**(服务器,v1.16.0)按 shard 冻结 copy-on-write view 后
  立即返回;后台线程写 snapshot,reactor tick 把 snapshot rename 和
  log reset 一并提交在相邻的 critical section 里。`BGSAVE` 之后发出的
  写继续落进旧 log 直到 swap,所以任何时刻 crash 不丢东西。
- **`Store::save_snapshot`**(嵌入式)跟 `SAVE` 一样同步,带同样的
  view-freeze 把戏:per-shard 锁只在 freeze + commit 期间持有,不覆盖
  磁盘写。

## fsync 策略(`appendfsync`)

控制 AOF flush 到磁盘的频率。默认 **`EverySec`**(对齐 Redis)。

| 策略 | 持久性 | 代价 |
|------|------|------|
| `Always` | 零丢失(每次写在回复前 fsync) | ~50% 吞吐 |
| `EverySec`(默认) | crash 丢 ≤ 1 秒的写 | 便宜 |
| `No` | OS pagecache 决定 | 最便宜 |

服务器端用 `appendfsync` 配置 key 设置(TOML 文件里 `always` /
`everysec` / `no`,也可 `CONFIG SET appendfsync …` 实时调),嵌入式用
`Config::with_appendfsync(AppendFsync::Always)`。

嵌入式模式下 `EverySec` 的 flush 窗由后台 TTL reaper tick 驱动(或者
manual reaper 模式时你的 `Store::tick` 调用)。

## TTL 持久化 —— 绝对截止时间(v1.8.1+)

TTL 持久化为**绝对 Unix 毫秒截止时间**(AOF 里的 `PEXPIREAT`,
snapshot 格式 v3 里的绝对字段)。一个 key 跨任意多次重启都保留原始的
过期时刻;进程 down 的时间会正确减去,deadline 已经过期的 key 在 load
时直接丢掉。

> v1.8.1 之前 TTL 存的是相对(`PEXPIRE <remaining>`),load 时锚到当
> 前时间,所以每次重启把 key 重置到一个全新的 full TTL。v1.8.1 修了
> (INC-2026-06-09)。旧的相对 `PEXPIRE` AOF 条目和 v2 snapshot 仍能
> 加载(按相对处理)—— 不需要迁移;新写都是绝对的。`EXPIREAT` /
> `PEXPIREAT` 也作为客户端命令暴露。

## AOF 重写 / 压缩

AOF 随每次写增长,包括同 key 反复覆盖。**重写**把它重建成能重构当前
keyspace 的最小命令集(每 key 一条 `SET`/`HSET`/…,带 TTL 的 key 再
加 `PEXPIREAT`),然后原子替换在线文件 —— 所以 10 000 次 `SET hot …`
压缩为单条 `SET hot <最新>`。

**手动**(始终可用):

- 服务器:`BGREWRITEAOF` —— v1.16.0 后台:`+OK` 在 shard 冻结
  keyspace 的 copy-on-write view 后返回(O(n)-shallow,每 key 几纳秒);
  per-shard 后台线程序列化,在磁盘写完成的下一个 reactor tick
  (~100 ms)内 swap 进新文件。看 `INFO persistence`(下文)的完成信号。
  每 shard 一个并发 job;在运行时落到的请求带 log 行跳过。
- 嵌入式:`Store::rewrite_aof() -> io::Result<Option<RewriteStats>>` ——
  从调用者视角同步(在原子 rename 后返回),但 per-shard 锁只在 view
  freeze 和最终 swap 期间持有;序列化过程中并发读写正常流动。

**自动**(Redis-style 阈值):当在线 AOF 比上一次重写时的大小增长了
`percentage`,**且**至少 `min_size` 字节时,触发重写。默认
**100% / 64 MiB**。

- 服务器:配置 key `auto_aof_rewrite_percentage` /
  `auto_aof_rewrite_min_size`(可 `CONFIG SET` 实时调);reactor tick 上检查。
- 嵌入式:`Config::with_auto_aof_rewrite(pct, min_size)`;后台 reaper
  tick 检查,或者 manual reaper 模式时你的 `Store::tick` 调用。
  `pct = 0` 关掉自动(只手动)。

每条重写路径对 keyspace **都不阻塞**(v1.16.0):锁(嵌入式)或 shard
线程(服务器)只在冻结 copy-on-write view 时持有 —— collection 值是
refcount 共享,所以 freeze 是 O(keys) 而非 O(bytes) —— 以及把完成的
文件 swap 进来时。序列化 + fsync 在 keyspace 在线的状态下跑;期间落
地的写 tee 进一个 diff buffer,在压缩 image 后追加,所以不丢任何东西。
瞬时代价是 view(每 key 几十字节)加上 rewrite 期间首次变更的
collection 的一次性 copy。如果 rewrite 跑到一半 crash,原 AOF 不受影响
(swap 是原子 `rename`),`<aof>.rewrite` 临时文件可以删。

## 崩溃恢复(启动时 AOF replay)

打开时,kevy 加载 `dump-<id>.rdb`(如有),然后 replay `aof-<id>.aof`:

- **干净** → 所有命令应用。
- **尾部截断**(crash 在 append 中途)→ 前缀 replay;部分尾帧丢掉。可
  恢复,除了那次未完成的写之外无数据丢失。
- **损坏帧**(例如非 kevy 字节写到了文件路径里) → 前缀 replay,坏尾
  丢掉,惹麻烦的字节搬到 `aof-<id>.aof.panic-quarantine.<unix_ts>` 防
  止它们卡未来启动。隔离的尾 *不*重新应用;你要从里面恢复东西自己手
  动 inspect。

每次 replay 输出一行总结(含 wall-clock 时间):

```
kevy: AOF /data/kevy/aof-0.aof replayed 145313 commands from 418261733 bytes in 247 ms (clean)
```

AOF 无上界,所以 replay 时间会跟着涨 —— 看这数字,用 auto-rewrite 把
它压住。

shard 布局迁移(改 `--threads` / `shards`,或者切换 cluster 路由)是
crash-idempotent:新 snapshot 落到 `.reshard` 临时名,持久的
`reshard.journal` 标记提交点,被打断的迁移在下一次启动时 roll forward。
源文件作为 `.premigration.<timestamp>` 备份保留。

## Introspection(服务器)

`INFO persistence` 报告应答 shard 的视图,每个 reactor tick(~100 ms)
刷新:

```
aof_enabled:1
appendfsync:everysec
aof_rewrite_in_progress:0     # 后台 save/rewrite 在跑
aof_rewrites_total:3          # 自打开以来完成的 AOF rewrite 数
```

`aof_rewrites_total` 增加(且 `in_progress` 回到 `0`)是异步
`BGREWRITEAOF` / `BGSAVE` 的完成信号。

## Introspection(嵌入式)

进程内模式没有给 `redis-cli` 用的 TCP endpoint,所以同样的信号在
`Store` 句柄上以方法形式提供:

```rust
store.dbsize();                 // 活 key 数
store.info();                   // KevyInfo { keys, used_memory, aof_bytes,
                                //            expire_pending, evictions, expired_keys }
store.ttl(key);                 // Option<Duration>(None = 没 key / 没 TTL)
store.ttl_ms(key);              // 原始 Redis PTTL:-2 没 key、-1 没 TTL、否则 ms
store.expire_pending_count();   // 带 TTL 的活 key 数(expire-set 大小)
store.used_memory();            // 驻留字节估算
store.expired_keys_total();     // 总过期(lazy + reaper)
store.evictions_total();        // 由 maxmemory 总驱逐
```

期望有 TTL 但 `expire_pending_count() == 0`,是 TTL 子系统没注册你的
key 的信号。

### 推送式 metrics

要持续监控(对比轮询 `info()`),注册一个 callback:

```rust
let cfg = Config::default()
    .with_persist("/data/kevy")
    .with_metric_sink(|m| match m {
        KevyMetric::Replay { commands, bytes, elapsed_ms } => { /* 启动 */ }
        KevyMetric::Rewrite { keys, before_bytes, after_bytes, elapsed_ms } => { /* 压缩 */ }
        _ => {}
    });
```

sink 在 AOF replay(启动)和每次 AOF rewrite(压缩)时触发。它在发射
线程(后台 rewrite 时是 reaper 线程)上同步运行,所以保持快。
`KevyMetric` 是 `#[non_exhaustive]` —— 用 `_` arm 接住保持前向兼容。

## **不**持久化的东西

- **Pub/sub** —— 频道、订阅、已发布消息都只在内存里;pub/sub 没有任
  何东西写进 AOF 或 snapshot。
- **阻塞命令 waiter**(`BLPOP`、阻塞 `XREAD`)—— 连接状态,不是数据。

## 文件命名参考

| 模式 | 意义 |
|------|------|
| `aof-<id>.aof` | shard `<id>` 的在线 AOF |
| `dump-<id>.rdb` | shard `<id>` 的二进制 snapshot |
| `shards.meta` | 记录 shard 数 + 路由方案 |
| `dump-<id>.rdb.tmp` | 进行中的 snapshot 写;陈旧的可删 |
| `aof-<id>.aof.rewrite` | 进行中的 AOF 重写/reset;陈旧的可删 |
| `dump-<id>.rdb.reshard` + `reshard.journal` | 进行中的 shard 布局迁移(下次启动 roll forward;**绝不**手删 journal) |
| `*.premigration.<unix_ts>` | 迁移前源备份 |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | 恢复时被搬开的损坏 AOF 尾 |
