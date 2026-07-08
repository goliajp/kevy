# 持久化

kevy 如何让数据扛过重启——AOF、快照、fsync 策略、重写/压实、崩溃恢复，以及让你把这一切随时看在眼里的内省接口。

## 何时需要这份文档

遇到下面这些情况时来查这一页：

- 为生产部署挑选耐久性策略（零丢失还是吞吐优先）。
- 为写密集负载估算磁盘占用和重放时间预算。
- 排查磁盘上冒出来的意外产物——隔离文件、过期残留的 `.rewrite` 临时文件、`.premigration.*` 备份。
- 把 `kevy_embedded::Store` 嵌进宿主应用，想弄清进程崩溃后什么能留下、什么留不下，以及在宿主内部怎么观测。
- 有个键的 TTL 在重启前后行为怪异。

如果只想要“`kill -9` 能不能扛住”的快速答案：能，默认策略下最多丢一秒的写入。

## 核心思路

每个 shard 在持久化目录下各有两个文件：一份记录全部写命令的追加式日志（`aof-<id>.aof`），和一份可选的二进制快照（`dump-<id>.rdb`）。单凭 AOF 就是一份完整的耐久记录；快照的唯一作用是给重放时间设上限。启动时 kevy 先加载快照（如有），再回放 AOF；快照成功后 AOF 会重置，两个文件合起来恰好覆盖完整历史一次。

启动时目录不存在就自动创建（v3.17）；路径创建失败会报成一条明确的启动错误，而不是让先碰到它的某个子系统抛一个裸 ENOENT。

## 实战示例

### 服务器模式

把下面的内容写进 `kevy.toml`，然后用 `kevy --config kevy.toml` 启动：

```toml
# kevy.toml
dir         = "/var/lib/kevy"
port        = 6379
threads     = 4
appendonly  = true

# AOF durability — see the knobs table below for the full set.
appendfsync                 = "everysec"   # always | everysec | no
auto_aof_rewrite_percentage = 100          # rewrite when AOF doubles since last rewrite
auto_aof_rewrite_min_size   = 67108864     # …and is at least 64 MiB
```

通过 RESP 用标准的 Redis 风格命令操作：

```text
$ redis-cli -p 6379 BGSAVE
Background saving started

$ redis-cli -p 6379 BGREWRITEAOF
Background append only file rewriting started

$ redis-cli -p 6379 INFO persistence
aof_enabled:1
appendfsync:everysec
aof_rewrite_in_progress:0
aof_rewrites_total:3
```

`CONFIG SET appendfsync always` 可以在线改策略，不用重启。

### 嵌入模式

在 `Cargo.toml` 里加入 crate：

```toml
[dependencies]
kevy-embedded = "*"
```

然后在 `main.rs`：

```rust
use std::time::Duration;
use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};

fn main() -> std::io::Result<()> {
    let cfg = Config::default()
        .with_persist("/var/lib/myapp/kevy")
        .with_appendfsync(AppendFsync::EverySec)
        .with_auto_aof_rewrite(100, 64 * 1024 * 1024)
        .with_metric_sink(|m| match m {
            KevyMetric::Replay { commands, bytes, elapsed_ms } => {
                eprintln!("kevy replay: {commands} cmds / {bytes} B in {elapsed_ms} ms");
            }
            KevyMetric::Rewrite { keys, before_bytes, after_bytes, elapsed_ms } => {
                eprintln!(
                    "kevy rewrite: {keys} keys, {before_bytes} -> {after_bytes} B in {elapsed_ms} ms"
                );
            }
            _ => {}
        });

    let store = Store::open(cfg)?;

    store.set("hello", b"world")?;
    store.pexpire("hello", Duration::from_secs(300))?;

    // Point-in-time snapshot. Returns after the file is on disk; per-shard
    // locks are held only for the view freeze and the final rename.
    store.save_snapshot()?;

    // On-demand AOF compaction. Same lock discipline as save_snapshot.
    let _stats = store.rewrite_aof()?;

    // Live introspection.
    let info = store.info();
    println!("{} keys, {} bytes AOF", info.keys, info.aof_bytes);

    Ok(())
}
```

默认配置下，新建的嵌入式 store 只写 AOF—— `save_snapshot` 跑过之前不会出现快照文件。这是预期行为：单凭 AOF 已足够重建键空间。

## 配置旋钮

### 耐久性与 AOF 增长

| 旋钮 | 服务器（TOML / `CONFIG SET`）| 嵌入式（`Config::…`）| 默认值 | 备注 |
|---|---|---|---|---|
| AOF fsync 策略 | `appendfsync`（`always` / `everysec` / `no`）| `with_appendfsync(AppendFsync::…)` | `EverySec` | 服务器侧可在线调整。 |
| AOF 开关 | `appendonly`（`true` / `false`）| 由 `with_persist(...)` 隐式开启 | `true`（服务器）；嵌入式在调用 `with_persist` 前关闭 | 关闭后跳过一切磁盘持久化。 |
| 自动重写百分比 | `auto_aof_rewrite_percentage` | `with_auto_aof_rewrite(pct, min)` 的第一个参数 | `100` | 设为 `0` 关闭自动重写。 |
| 自动重写最小体积 | `auto_aof_rewrite_min_size` | `with_auto_aof_rewrite(pct, min)` 的第二个参数 | `67108864`（64 MiB）| 两个阈值同时满足才触发自动重写。 |
| 持久化目录 | `dir` / 环境变量 `KEVY_DIR` | `with_persist(path)` | 服务器 `./data`；嵌入式无 | 每个 kevy 实例一个目录。 |
| reactor / reaper 节拍 | reactor tick，约 100 ms | 后台 reaper，或自行调用 `Store::tick` | 约 100 ms | 驱动 `EverySec` 刷盘、自动重写检查、TTL 清理。 |

### 触发面

| 动作 | 服务器 | 嵌入式 | 阻塞形态 |
|---|---|---|---|
| 同步快照 | `SAVE` | `Store::save_snapshot()` | 文件落盘后返回；锁只在冻结 + rename 期间持有。 |
| 后台快照 | `BGSAVE` | 在工作线程里调用 `save_snapshot` | 立即返回；磁盘写完后一个 reactor tick 内提交落地。 |
| AOF 重写 | `BGREWRITEAOF` | `Store::rewrite_aof()` | 原子 rename 后返回；序列化期间键空间照常在线。 |
| 在线调整 fsync | `CONFIG SET appendfsync everysec` | 重建 `Config` | 无 |

### fsync 策略语义

| 策略 | 耐久性 | 代价 |
|---|---|---|
| `Always` | 零丢失——每次写入先 fsync 再回复 | 吞吐约砍半 |
| `EverySec`（默认）| 崩溃最多丢约 1 秒的写入 | 开销小 |
| `No` | 交给 OS 页缓存刷盘 | 开销最小 |

## 取舍与限制

**各策略的吞吐与数据丢失。**`Always` 让每条回复都等 `fsync` 完成，是唯一能在 `kill -9` 下做到零命令丢失的策略，代价是在典型 NVMe 上把 SET 密集的吞吐砍掉约一半。`EverySec` 由后台每秒刷一次盘，崩溃最多丢这一秒窗口内的写入——之所以选它当默认，正因为它与 Redis 的取舍一致，且丢失窗口通常可以接受。`No` 交给内核定夺：吞吐最高，但崩溃可能丢掉还留在页缓存里的一切，时间跨度可能达数秒。

**AOF 重放成本与快照加载成本。**没有快照时，启动耗时随 AOF 字节数线性增长：本地 NVMe 上，4 GiB 的 AOF 几秒钟回放完，40 GiB 就要一分钟以上。快照能封住这个上限——加载只是一次流式读，外加快照之后那一小段 AOF 尾巴——但代价是一次短暂的视图冻结（O(keys)，每键纳秒级，因为集合值靠引用计数共享），外加快照落盘期间首次改动的集合各拷贝一次。写密集负载下，更推荐靠自动重写压住 AOF 体积，而不是定期跑 `BGSAVE`：重写给出同样的启动时间上限，还省掉第二个文件的管理。

**后台任务并发。**每个 shard 同一时刻最多跑一个后台保存或重写。任务进行中再来的重复请求会记一条日志然后跳过，绝不排队。

**TTL 持久化。**TTL 以绝对的 Unix 毫秒截止时间落盘（AOF 里写 `PEXPIREAT`，快照格式里是一个绝对时间字段），所以无论重启多少次，键都保持原来的过期时刻，进程停机的时长也能正确扣除。记录相对剩余时间的旧版 AOF 仍可加载（载入时按相对时间处理）；新写入一律是绝对时间。`EXPIREAT` 和 `PEXPIREAT` 都作为客户端命令开放。

**shard 布局变更崩溃幂等。**修改 `--threads` / `shards` 时，新快照先写到 `.reshard` 临时名下，经由一份耐久的 `reshard.journal` 提交；迁移若中断，下次启动会向前滚完。源文件保留为 `.premigration.<unix_ts>` 备份；journal 是提交点，绝不能手工删除。

**哪些东西不持久化。**Pub/sub 频道、订阅和未投递的消息只活在内存里。`BLPOP` 之类阻塞命令的等待者和阻塞式 `XREAD` 属于连接状态，不是数据。这两类都不写 AOF、不进快照，也不参与回放。

## FAQ

### AOF 文件一直在涨——怎么压实？

在服务器上跑 `BGREWRITEAOF`，嵌入式模式下调用 `Store::rewrite_aof()`。重写把日志重建为能还原当前键空间的最小命令集——每个键一条 `SET` / `HSET` 等，带 TTL 的键再加一条 `PEXPIREAT`——然后原子地换入新文件。对 `hot` 的一万次覆盖会坍缩成一条 `SET hot <latest>`。

无人值守的运维场景，自动重写保持默认即可——相对上次重写体积增长 100%，且不低于 64 MiB——reactor 会自行触发压实。设 `auto_aof_rewrite_percentage = 0` 则关闭自动重写，完全手动驱动。

重写不阻塞键空间：序列化和 `fsync` 进行时读写照常流动，期间落地的写入会 tee 进一个 diff 缓冲区，最后追加到压实后的镜像上。重写中途崩溃不影响原 AOF（换入是一次原子 `rename`），残留的 `aof-<id>.aof.rewrite` 临时文件删掉即可。

### 能彻底关掉持久化吗？

可以，两条路：

- **服务器：**在 `kevy.toml` 里设 `appendonly = false`（或省略 `--dir`）。服务器就是一个纯内存缓存；不会创建任何 `aof-*` 或 `dump-*` 文件。
- **嵌入式：**构造 `Config` 时不调用 `with_persist(...)`。`Store::open` 把整个键空间放在内存里；`save_snapshot` 和 `rewrite_aof` 在 API 层面变成无操作（或返回一个提示未配置持久化目录的错误）。

如果想要持久化、又希望两次快照之间 AOF 完全不增长，这种组合不支持——kevy 的耐久模型是 AOF 优先，快照只为限定 AOF 重放，不是 AOF 的替代品。

### 高写入负载下做一次快照要付出什么？

阻塞部分很小。每个 shard 的键空间冻结是 O(keys) 而非 O(bytes)——集合值有引用计数，和在线 store 共享——百万键的 shard 冻结只要个位数毫秒。序列化本身在键空间在线时进行，写入不会暂停。

真正的临时开销在内存。快照写出期间发生改动的集合（list、hash、set、sorted-set）各克隆一次，好让在线 store 不打扰冻结视图、继续前进。负载以 `SET` 普通字符串键为主时，这点额外内存可以忽略；若集中用 `HSET` / `LPUSH` 打少数几个巨型集合，这些集合的常驻内存可能短暂翻倍。

快照成功后还会重置 AOF——日志原先承载的内容如今全在快照里，日志只从冻结之后落地的写入重新记起。之后重启加载快照 + 日志，不会把历史应用两遍。

### 下次启动时按什么顺序恢复？

每个 shard 依次执行：

1. **加载快照。**若 `dump-<id>.rdb` 存在，流式载入键空间。已过期的 TTL 在加载时直接丢弃。
2. **回放 AOF。**从 `aof-<id>.aof` 开头逐帧应用。
3. **处理尾部。**文件完好就全量应用。尾部截断（追加到一半崩溃）则丢掉残缺的末帧，应用前面的部分。遇到损坏帧，坏字节会挪到 `aof-<id>.aof.panic-quarantine.<unix_ts>`，不再阻碍后续启动，然后应用前缀。隔离出去的尾巴永远不会重新应用；想从里面抢救内容就手工检查。
4. **打出一行摘要日志**，含挂钟耗时：

   ```text
   kevy: AOF /data/kevy/aof-0.aof replayed 145313 commands from 418261733 bytes in 247 ms (clean)
   ```

5. **回放 `reshard.journal`**，把中断的 shard 布局迁移向前滚完。

盯住这行重放耗时，用自动重写把它压在预算内——重放时间随未重写的 AOF 体积线性增长。

### 嵌入式宿主进程内部怎么监控持久化？

两个入口。

**轮询。**`store.info()` 返回 `KevyInfo` 结构体，字段有 `keys`、`used_memory`、`aof_bytes`、`expire_pending`、`evictions`、`expired_keys`。同样的信息也有更细粒度的方法：

```rust
store.dbsize();                 // live key count
store.ttl(key);                 // Option<Duration> (None = no key / no TTL)
store.ttl_ms(key);              // Redis PTTL semantics: -2 no key, -1 no TTL, else ms
store.expire_pending_count();   // live keys carrying a TTL
store.used_memory();            // resident-bytes estimate
store.expired_keys_total();     // total expired (lazy + reaper)
store.evictions_total();        // total evicted by maxmemory
```

期望有 TTL 却看到 `expire_pending_count() == 0`，是 TTL 子系统没登记上你那些键的经典信号。

**推送。**注册 `Config::with_metric_sink(...)`，AOF 重放（启动时）和每次 AOF 重写（压实）都会送来 `KevyMetric` 事件。sink 在发出事件的线程上同步执行（后台重写发自 reaper 线程），回调要快。`KevyMetric` 标了 `#[non_exhaustive]`——匹配时永远留一个 `_` 分支，保证向前兼容。

### 持久化目录里每个文件都是什么？

| 模式 | 含义 |
|---|---|
| `aof-<id>.aof` | shard `<id>` 的在线 AOF。 |
| `dump-<id>.rdb` | shard `<id>` 的二进制快照。 |
| `shards.meta` | 记录的 shard 数量与路由方案。 |
| `dump-<id>.rdb.tmp` | 写出中的快照。确认陈旧后可安全删除。 |
| `aof-<id>.aof.rewrite` | 进行中的 AOF 重写/重置。确认陈旧后可安全删除。 |
| `dump-<id>.rdb.reshard` + `reshard.journal` | 进行中的 shard 布局迁移。下次启动向前滚完；journal 绝不能手工删除。 |
| `*.premigration.<unix_ts>` | 迁移前的源文件备份，留作回滚。 |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | 恢复时隔离出来的损坏 AOF 尾部。想抢救内容就手工检查；kevy 不会重新应用它。 |
| `elect.meta`（+ 瞬态的 `elect.meta.tmp`）| 选举耐久性（v3.15）：选举器的 `(epoch, votedFor)` 二元组，在任何投票应答离开节点*之前*先持久化，崩溃重启因此绝不会重复投票。写法是 tmp + fsync + rename——保存中途崩溃只会留下旧值或新值，绝无撕裂的文件。仅在配置了 `[cluster]` 多数派时出现。 |

## 耐久性契约（v2.1）

按 `appendfsync` × 写入路径，说明“调用返回 OK”各自保证了什么。“durable”= 已落到稳定存储（`fdatasync` 已完成）；“windowed”= 还在 OS 页缓存里，只有*机器*（而不只是进程）在窗口内死掉才会丢。

| 写入路径 | `always` | `everysec` | `no` |
|---|---|---|---|
| 服务器命令回复 | 回复离开 shard 前已 durable（按批次组提交）| windowed ≤ 1 s | 由 OS 定节奏 |
| 嵌入式门面操作（`set`、`zadd`、…）| 返回即 durable | windowed ≤ 1 s | 由 OS 定节奏 |
| 嵌入式 `atomic` / `atomic_all_shards` 块 | 提交即 durable（每个触及的 shard 一次 fsync）| windowed ≤ 1 s | 由 OS 定节奏 |
| 嵌入式 `Pipeline::commit` | 返回即 durable，fsync 按 shard 合批 | windowed ≤ 1 s | 由 OS 定节奏 |
| …以上任一 + **`Store::fsync_aof()`** | 无操作 | **屏障处即 durable** | **屏障处即 durable** |

`Store::fsync_aof()` 是逐写入粒度的耐久性逃生口（Postgres 按事务 `synchronous_commit` 那一路）：部署跑 `everysec` 换吞吐，再把屏障放在少数几笔“一经确认就必须扛住机器崩溃”的写入之后。代价：每个脏 shard 一次 `fdatasync`。

进程崩溃（SIGKILL）在 `always` 下绝不丢已确认的写入，其他策略最多丢一个 fsync 窗口；AOF 尾巴在下次打开时回放，撕裂的末帧会隔离（`panic-quarantine`），绝不静默应用。

## 原子性章程（嵌入式 serving-store，v2.1）

- **`Store::atomic(body)`**——单 shard 事务：闭包期间持有该 shard 的写锁，闭包内的读能看到自己刚写的内容，AOF 追加先攒着、**提交时一次 fsync** 落盘（`always` 下）。事务触及的所有键必须哈希到同一个 shard——所以写模式跨任意键时，serving-store 的钦定配置就是 **1 shard**：原子性完整保留，又不付跨 shard 协调的成本。1 shard 配置的天花板是单核写吞吐；实测数字见 `bench/REPORT.md`。
- **`Store::atomic_all_shards(body)`**——多 shard 事务：按 shard 索引顺序拿下**所有** shard 的写锁（顺序确定 = 不会死锁），返回时按 shard 提交 AOF 批次。代价：闭包期间阻塞其他所有读写——用于维护跨 shard 不变量，别当默认写路径。
- **`Store::pipeline()`**——**不**原子：每个操作各自拿锁，其他写者会穿插进来。它只负责合批 fsync（N 个操作 → 至多 shard 数次 fsync），仅此而已。
- 两种原子形式都把条件操作（`ZADD GT`、`SPOP`）的**效果**记成无条件 verb，重放和副本应用因此在构造上就是确定性的。

## 恢复点（v2.3）

启用变更 feed（`[feed] enabled = true`，见 [cdc.md](../cdc.md)）后，每份快照都记下采集那一刻的 feed 游标——与快照数据本身在同一个禁追加窗口里冻结。由此得到恢复点契约：

> **快照 S + 从 S 所记游标起的 feed 帧 = 之后任意游标处的精确状态。**

`kevy_persist::read_snapshot_cursor(path)` 能把游标读回来（v2.3 之前的快照返回 `None`——格式 v4 及更早不携带游标，但仍可完整加载）。这份契约的可执行形式是 `bench/restore-drill.sh`，作为一条 `diskgate` 项运行：写入 → SAVE → 再写入 → kill → 只用 dump 恢复 → 回放捕获的 feed 帧 → 逐键逐字节校验。

范围说明：feed 窗口就是内存中的 backlog。老过窗口的帧已经不在了——快照若老过窗口所及，就只是一次普通的快照恢复（S 时刻的状态），当不了 PITR 基点。依赖精确时间点恢复的话，打快照的频率至少要跟上窗口的翻转速度。

## 复制链路上的快照（v3.15）

副本掉出 backlog 窗口时，主节点内联推送给它的就是同一种快照格式（见 [replication.md](replication.md)）。有一个语义值得记住：推送过来的快照会**替换**副本的本地状态，而不是合并——副本加载前先清空自己的键空间。这是刻意设计：重新加入的前主节点若带着分叉后缀（从未复制出去的写入），重同步就必须真正丢掉这条分叉，而不是在只做 upsert 的加载下把它留成残渣。
