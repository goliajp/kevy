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
[server]
data_dir = "/var/lib/kevy"
port     = 6379
threads  = 4

[persistence]
aof = true
# AOF durability — see the knobs table below for the full set.
appendfsync                 = "everysec"   # always | everysec | no
auto_aof_rewrite_percentage = 100          # rewrite when the AOF doubles since the last rewrite
auto_aof_rewrite_min_size   = "64mb"       # …and is at least this big
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

fn main() -> kevy_embedded::KevyResult<()> {
    let cfg = Config::default()
        .with_persist("/var/lib/myapp/kevy")
        .with_appendfsync(AppendFsync::EverySec)
        .with_auto_aof_rewrite(100, 64 * 1024 * 1024)
        .with_metric_sink(|m| match m {
            KevyMetric::Replay { commands, bytes, elapsed_ms, dropped_bytes, corrupt } => {
                eprintln!("kevy replay: {commands} cmds / {bytes} B in {elapsed_ms} ms");
                if dropped_bytes > 0 || corrupt {
                    eprintln!("kevy replay dropped {dropped_bytes} B (corrupt: {corrupt}) — ALERT");
                }
            }
            KevyMetric::Rewrite { keys, before_bytes, after_bytes, elapsed_ms } => {
                eprintln!(
                    "kevy rewrite: {keys} keys, {before_bytes} -> {after_bytes} B in {elapsed_ms} ms"
                );
            }
            _ => {}
        });

    let store = Store::open(cfg)?;

    store.set(b"hello", b"world")?;
    store.expire(b"hello", Duration::from_secs(300))?;

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
| AOF 开关 | `aof`（`true` / `false`）| 由 `with_persist(...)` 隐式开启 | `true`（服务器）；嵌入式在调用 `with_persist` 前关闭 | 关闭后跳过一切磁盘持久化。 |
| 自动重写百分比 | `auto_aof_rewrite_percentage` | `with_auto_aof_rewrite(pct, min)` 的第一个参数 | `100` | 设为 `0` 关闭自动重写。 |
| 自动重写最小体积 | `auto_aof_rewrite_min_size` | `with_auto_aof_rewrite(pct, min)` 的第二个参数 | `67108864`（64 MiB）| 两个阈值同时满足才触发增长规则。 |
| 自动重写绝对上限 | `auto_aof_rewrite_bytes` | `with_auto_rewrite_bytes(n)` | `0`（关）| 独立触发器：AOF 超过 `n` 字节即重写，与增长比例无关。可在线调整（`CONFIG SET auto-aof-rewrite-bytes`）。 |
| 自动重写陈旧度 | `auto_aof_rewrite_interval_secs` | `with_auto_rewrite_interval(d)` | `0`（关）| 独立触发器：距上次重写超过该时长且日志有增长即重写。可在线调整。 |
| resync 回放 | `replay_resync`（`[persistence]`）| `with_replay_resync(true)` | `false`（strict）| 仅启动时生效。文件中部损坏时恢复其后的完好尾巴，而不是停在损坏处——见 resync 一节。 |
| 持久化目录 | `data_dir` / 环境变量 `KEVY_DIR` | `with_persist(path)` | 服务器 `./data`；嵌入式无 | 每个 kevy 实例一个目录。 |
| reactor / reaper 节拍 | reactor tick，约 100 ms | 后台 reaper，或自行调用 `Store::tick` | 约 100 ms | 驱动 `EverySec` 刷盘、自动重写检查、TTL 清理。 |

### 触发面

| 动作 | 服务器 | 嵌入式 | 阻塞形态 |
|---|---|---|---|
| 同步快照 | `SAVE` | `Store::save_snapshot()` | 文件落盘后返回；锁只在冻结 + rename 期间持有。 |
| 后台快照 | `BGSAVE` | 在工作线程里调用 `save_snapshot` | 立即返回；磁盘写完后一个 reactor tick 内提交落地。 |
| AOF 重写 | `BGREWRITEAOF` | `Store::rewrite_aof()` | 原子 rename 后返回；序列化期间键空间照常在线。 |
| 在线调整 fsync | `CONFIG SET appendfsync everysec` | 重建 `Config` | 无 |
| 有序停机 | `SHUTDOWN [SAVE\|NOSAVE]`（或 SIGTERM） | drop 最后一个 `Store` clone | 逐 shard 排空：在途持久化任务落地、AOF 尾巴强制 fsync，然后进程退出。`SAVE` 额外为每个 shard 拍一张最终快照。不发送回复——客户端观察到连接关闭（Redis 行为）。 |

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
3. **处理尾部。**文件完好就全量应用。撕裂或损坏的记录让回放停在它之前最后一条完整记录；随后 open 把丢弃区复制到 `aof-<id>.aof.corrupt-quarantine.<unix_ts>`（fsync 过——文件中部损坏之后的区域大多是良构记录，这份副本是找回它们的唯一途径），并在首次追加之前**把文件截断到最后一条完整记录**，让新写入紧接在可回放前缀之后，而不是落在坏字节后面（否则下次回放又会停在那里，把它们静默变成孤儿）。隔离出去的字节永远不会重新应用；想抢救就手工检查。若隔离副本本身写不出来（如磁盘满），open 直接失败、文件原样保留——kevy 绝不销毁你字节的唯一副本。开启 `replay_resync` 时行为见 resync 一节。
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
| `aof-<id>.aof.corrupt-quarantine.<unix_ts>` | 恢复时隔离出来的不可回放区域（撕裂尾巴，或文件中部损坏记录之后的一切）。想抢救内容就手工检查；kevy 不会重新应用它。 |
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

进程崩溃（SIGKILL）在 `always` 下绝不丢已确认的写入，其他策略最多丢一个 fsync 窗口；AOF 尾巴在下次打开时回放，撕裂的末记录在打开时截掉（丢弃区先复制到隔离文件），绝不静默应用（完整状态机见下面的崩溃一致性契约）。

**有序停机**（`SHUTDOWN` 或 SIGTERM）在任何策略下都零丢失：排空过程会在退出前强制 fsync AOF 尾巴，所以崩溃可能丢掉的 `everysec` 窗口对干净停机不适用。

## 崩溃一致性契约（v4）

打开路径是每个 shard 一台固定状态机——**open → verify → replay → verdict → repair → append**——每种 verdict 都带一条硬丢失上界。`crashgate`（`bench/crashgate.sh`）逐格执行这张表：SIGKILL 矩阵（追加中 / 重写中 / 快照中 / feed 发射中 × fsync 策略 × shard 数）加上注入的撕尾 / 文件中部 / payload 损伤。

| 崩溃留下什么 | verdict | 回放恢复什么 | 硬丢失上界 |
|---|---|---|---|
| 干净文件 | `clean` | 全部 | 零 |
| 撕裂末记录（追加中被杀）| 截尾 | 每条完整记录 | 撕裂记录 + 未 fsync 窗口（`always`：仅撕裂记录）|
| 零填充尾巴（掉电 + 未 fsync 页）| 截尾 | 每条完整记录 | 同上 |
| 文件中部损坏记录 | 停在该记录（strict，默认）| 该记录之前的前缀；丢弃区隔离保存 | 该记录之后的区域——**或**开启 `replay_resync` 后仅损坏区本身：其后的完好尾巴被恢复（见 resync 一节）|
| 记录 payload 内位翻转 | CRC 不匹配 → 损坏记录（v2 文件）| 不应用任何被污染的值——记录被拒绝 | 同上行；v1 时代的文件不带校验和，位翻转会静默重放，直到首次重写升格为 v2 |

**无黑洞不变量**（3.18 事故的修复，由 crashgate 把守）：打开时的截断发生在**首次追加之前**，回放停止点绝不跨重启回退——崩溃后重启写下的数据，一定能活过下一次重启。

**多 shard 偏斜。** 每个 shard 独立拥有 `aof-<id>.aof`、独立修复，崩溃后不同 shard 可能各自恢复到略有差异的时刻（各在各的丢失上界内）。kevy 不为独立写入承诺跨 shard 原子性——`atomic`/`atomic_all_shards` 块在提交时按触及的 shard fsync，是一组写入必须一起落地时该用的工具。

**事务标记只属于事务。**流水线批不是事务（Redis 的 pipelining 明确非原子），所以 reactor 批不带标记（`always` 下只共享一次 fsync；4.x 线上曾有一段时间每个单命令批都要付约 65 B 的标记对，被磁盘门禁抓出后修正）。服务器侧的 `MULTI`/`EXEC` 在连接所属 shard 上为排队命令成组打标；扇出到其它 shard 的命令落进各自的日志、逐条独立。因此跨 shard `EXEC` 的崩溃原子性是按 shard 的，不是全局的——与 Redis 自己「EXEC 内运行时错误不回滚其它命令」的精神一致。上文的全有或全无保证属于嵌入式 `atomic()` 家族，它的写入按构造就是单 shard 的。

**feed（CDC）只在内存中，且跑在磁盘前面。** feed backlog 不会在打开时从 AOF 重建；重启后只有 `(generation, offset)` 游标存活。帧在 apply 时刻发射，**早于**记录同一笔写入的 AOF 字节被 fsync——`everysec` 下消费者可能观测到最多约 1 秒后被崩溃回滚的写入（`always`：零；`no`：无上界）。崩溃会 bump feed generation，所以所有崩溃前游标都会收到 `-FEEDRESYNC` / `FeedError::Resync`，消费者必须从恢复后的 store 重新扫描重建。只有覆盖某帧的 fsync 窗口关闭后，才能把它当成 durable 事实——对未 durable 帧采取的副作用，resync 无法召回。

**副本对未 fsync 的帧不持有耐久性主张。** 主节点 unclean 重启后会按未 fsync 后缀回滚并 bump feed generation；应用过被回滚写入的副本处于领先位，重连时其分叉历史经全量快照重同步丢弃。重连握手携带副本的 generation（v4），generation 不匹配时主节点拒绝按 offset 续传——无论副本隔多久重连，都不可能被静默喂入新历史的同号 offset（见 [replication.md](replication.md)）。

## AOF 记录格式（v2，`KEVYAOF2`）

自 4.0 起，新 AOF 文件以 `KEVYAOF2\n` magic 开头，每条命令都是一条带校验和的记录：

```text
[payload_len: u32 LE][crc32c: u32 LE][payload: RESP multibulk 命令]
```

这层信封买到的东西，每一样都对应一类 v1 格式处理不了的事故：

- **完整性。** `crc32c`（Castagnoli，aarch64 与 SSE4.2 x86-64 上走硬件指令）覆盖 payload。翻转一个位——盘介质衰减、坏线缆、被截断又覆写的页——校验即失败，记录被拒绝，而不是把污染值重放进键空间。v1 完全没有完整性校验：位翻转会静默重放。
- **确定性记录边界。** 长度前缀不解析 RESP 就能给日志分帧，撕裂尾巴靠算术检测（字节数少于头部承诺），而不是指望解析器恰好噎住。
- **确定性 resync。** 损坏区之后，下一条记录边界可以被重新找到并*验证*（长度 + CRC + 恰好一条良构命令三者必须同时吻合）——这就是下面 resync 回放的基础。

**兼容契约：**

- 4.0 二进制永久可读 v1（`KEVYAOF1`）文件。3.x 数据目录零迁移打开。
- 单个文件内绝不混格式：向既有 v1 文件的追加保持 v1。
- 新文件、截断、所有重写输出都是 v2。首次重写（自动或 `BGREWRITEAOF`）因此把 v1 文件升格为 v2——此后 3.x 二进制无法再读它。降级窗口与运维顺序见 [UPGRADING.md](UPGRADING.md)。

每条记录的开销是 8 字节；对 mailrs 形状的负载（混合小命令），磁盘成本实测在低个位数百分比，CRC 在两种服务器架构上都走硬件指令。

### resync 回放——恢复完好的尾巴

默认（strict）回放停在第一条损坏记录：前缀被应用，之后的一切——大多是良构记录——被隔离并从活文件中丢弃。这是诚实的默认值（出现损坏区意味着*有什么*出了问题；拒绝猜测最安全），但对只有一小块损伤的大日志来说，它放弃了可证明完好的数据。

`replay_resync` 选择把它们找回来：

```toml
[persistence]
replay_resync = true
```

（嵌入式用 `Config::with_replay_resync(true)`，手工搭 runtime 用 `Runtime::with_replay_resync(true)`；该设置仅启动时生效——回放先于第一次在线配置 tick。）

resync 模式下，回放跳过损坏区：向前扫描，直到长度前缀、CRC **和**恰好一条良构命令的解析三者同时吻合的位置（伪接受需要同时骗过三者——每个候选偏移约 2⁻³²），然后从那里继续应用。每段被跳过的区间都会上报——persist 层的 `ReplayReport::resynced_ranges`、`Store::open_report()` 上的 `OpenReport::resynced_bytes`——且 `corrupt` 标志保持竖起：resync 恢复数据，但不宣布文件健康。

修复语义与 strict 不同：磁盘文件只在**最后一条**可恢复记录之后截断（尾巴隔离保存）；内部损坏区留在原位——每次启动都再跳一遍——直到下一次重写把文件压实。“恢复完好尾巴”的保证是可执行的：`crashgate` 的文件中部 splice 格（mailrs 损伤形状，231 MB 级日志中间剪掉 8 字节）必须报告损伤之后的每条记录都被恢复。

什么时候保持关闭：如果宿主把*任何*损坏都当成切换副本或从备份恢复的理由，strict 给你最响、最早的停止。什么时候打开：单节点部署、AOF 是唯一副本——mailrs 的姿态——而“8 字节 splice 吞掉三天写入”是更糟的结局时。

## 原子性章程（嵌入式 serving-store，v2.1）

- **`Store::atomic(body)`**——单 shard 事务：闭包期间持有该 shard 的写锁，闭包内的读能看到自己刚写的内容，AOF 追加先攒着、**提交时一次 fsync** 落盘（`always` 下）。事务触及的所有键必须哈希到同一个 shard——所以写模式跨任意键时，serving-store 的钦定配置就是 **1 shard**：原子性完整保留，又不付跨 shard 协调的成本。1 shard 配置的天花板是单核写吞吐；实测数字见 `bench/REPORT.md`。
- **`Store::atomic_all_shards(body)`**——多 shard 事务：按 shard 索引顺序拿下**所有** shard 的写锁（顺序确定 = 不会死锁），返回时按 shard 提交 AOF 批次。代价：闭包期间阻塞其他所有读写——用于维护跨 shard 不变量，别当默认写路径。
- **`Store::pipeline()`**——**不**原子：每个操作各自拿锁，其他写者会穿插进来。它只负责合批 fsync（N 个操作 → 至多 shard 数次 fsync），仅此而已。
- 两种原子形式都把条件操作（`ZADD GT`、`SPOP`）的**效果**记成无条件 verb，重放和副本应用因此在构造上就是确定性的。

## 恢复点（v2.3）

启用变更 feed（`[feed] enabled = true`，见 [cdc.md](cdc.md)）后，每份快照都记下采集那一刻的 feed 游标——与快照数据本身在同一个禁追加窗口里冻结。由此得到恢复点契约：

> **快照 S + 从 S 所记游标起的 feed 帧 = 之后任意游标处的精确状态。**

`kevy_persist::read_snapshot_cursor(path)` 能把游标读回来（v2.3 之前的快照返回 `None`——格式 v4 及更早不携带游标，但仍可完整加载）。这份契约的可执行形式是 `bench/restore-drill.sh`，作为一条 `diskgate` 项运行：写入 → SAVE → 再写入 → kill → 只用 dump 恢复 → 回放捕获的 feed 帧 → 逐键逐字节校验。

范围说明：feed 窗口就是内存中的 backlog。老过窗口的帧已经不在了——快照若老过窗口所及，就只是一次普通的快照恢复（S 时刻的状态），当不了 PITR 基点。依赖精确时间点恢复的话，打快照的频率至少要跟上窗口的翻转速度。

## 复制链路上的快照（v3.15）

副本掉出 backlog 窗口时，主节点内联推送给它的就是同一种快照格式（见 [replication.md](replication.md)）。有一个语义值得记住：推送过来的快照会**替换**副本的本地状态，而不是合并——副本加载前先清空自己的键空间。这是刻意设计：重新加入的前主节点若带着分叉后缀（从未复制出去的写入），重同步就必须真正丢掉这条分叉，而不是在只做 upsert 的加载下把它留成残渣。

对带本地 AOF 的副本，这个“替换而非合并”语义有一个后果：清空 + 快照加载绕过了提交路径，所以加载完成后副本会同步地用重同步后的键空间重写本地 AOF（4.0）。否则本地日志仍描述重同步前的历史，主节点不可达时的重启会端出一个从错误底座拼出来的状态。

## 运维手册

按事故形状整理的清单。这里每一行背后都有一道门（`crashgate`、`repligate`、`diskgate`）或本文点名的测试。

**停 kevy。** 优先有序停机——服务器用 `SHUTDOWN` / SIGTERM，嵌入式 drop 最后一个 `Store` 克隆或调 `Store::shutdown()`。排空会强制 fsync 每个 AOF 尾巴，所以有序停机在任何 fsync 策略下都零丢失；feed 也会写下干净停机标记，消费者与副本续接时不 bump generation。`kill -9` 是*可存活的*（crashgate 的整个矩阵就是它），但会付出 fsync 窗口，并打断 feed/副本连续性（generation bump → 消费者重建、副本快照重同步）。

**每次（重）启动后，读 verdict。** 三个等价面，至少接一个进健康检查：

- 每 shard 的启动日志行——`kevy: AOF … replayed N commands from M bytes in T ms (clean)`。凡不是 `(clean)`，都伴随一条 WARN，点名丢弃字节数与隔离文件路径。
- 服务器 `INFO persistence`：`aof_last_open_dropped_bytes` 与 `aof_last_open_corrupt`——非零表示本次启动恢复的比文件里有的少。对它设告警；那场三天静默丢失的事故，正是这个信号只活在 stderr 里。
- 嵌入式 `Store::open_report()`（C ABI 走 `kevy_open_report`）：`dropped_bytes`、`corrupt`、`quarantine_paths`、`resynced_bytes` 全是数据——把一次坏启动变成一次被拒绝的部署，而不是一行没人读的日志。

**若启动报告了丢弃。** 丢弃区在 `aof-<id>.aof.corrupt-quarantine.<unix_ts>` 里，逐字节原样。按此顺序决策：（1）副本或备份里有这些写入，从那边恢复；（2）没有，考虑带 `replay_resync = true` 启动一次——文件中部损坏时它恢复完好尾巴并上报跳过区间；（3）手工打捞隔离文件（内容大多是良构记录 / RESP）。事故关闭前保留隔离文件；kevy 绝不会删它。

**重写节奏。** 自动重写就是启动时间与事故半径的上界：回放耗时、隔离爆炸半径、resync 跳跃成本都随未重写日志的体积伸缩。增长对（`auto_aof_rewrite_percentage` / `min_size`）是默认项；磁盘或回放预算是硬约束时加绝对上限（`auto_aof_rewrite_bytes`），写入稀疏、日志翻不了倍却攒了几周历史的部署加陈旧度触发器（`auto_aof_rewrite_interval_secs`）。三者独立，先到先触发。

**磁盘满与隔离失败会大声失败。** 打开时若写不出隔离副本（如磁盘满），open 直接失败、AOF 原样保留，而不是把你数据的唯一副本截掉。腾出空间，再重新打开。
