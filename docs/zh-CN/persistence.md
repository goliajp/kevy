# 持久化

kevy 如何在重启之间保留数据 —— AOF、快照、fsync 策略、重写/压实、崩溃恢复,以及让你随时观察这一切的内省接口。

## 何时需要这份文档

下列情形应当查阅本页:

- 为生产部署挑选耐久性策略(零丢失 vs 吞吐)。
- 为写密集型负载估算磁盘占用与重放时长预算。
- 调查磁盘上意外出现的产物 —— 隔离文件、过期的 `.rewrite` 临时文件、`.premigration.*` 备份。
- 把 `kevy_embedded::Store` 嵌入宿主应用,并想知道进程崩溃后什么会留下、什么不会,以及如何在宿主进程内部观测它。
- 某个键的 TTL 在重启之间表现异常。

如果你只想要一个"`kill -9` 之后还能不能扛?"的快速答案:能扛,在默认策略下最多丢失一秒钟的写入。

## 核心思路

每个 shard 在持久化目录下拥有两个文件:一份对所有改动命令的追加式日志(`aof-<id>.aof`),以及一份可选的二进制快照(`dump-<id>.rdb`)。仅靠 AOF 本身就构成完整的耐久记录;快照只是用来限定重放时间。启动时 kevy 先加载快照(如果存在)再回放 AOF;一次成功的快照之后 AOF 会被重置,因此两个文件合在一起恰好覆盖一次完整历史。

## 实际示例

### 服务器模式

把下面的内容写到 `kevy.toml`,然后用 `kevy --config kevy.toml` 启动:

```toml
# kevy.toml
dir         = "/var/lib/kevy"
port        = 6379
threads     = 4
appendonly  = true

# AOF 耐久性 —— 完整的旋钮表见下文。
appendfsync                 = "everysec"   # always | everysec | no
auto_aof_rewrite_percentage = 100          # AOF 相对上次重写增长一倍时触发重写
auto_aof_rewrite_min_size   = 67108864     # ...并且至少 64 MiB
```

用标准 Redis 风格命令操作 RESP 接口:

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

`CONFIG SET appendfsync always` 可在不重启的情况下实时调整策略。

### 嵌入模式

在 `Cargo.toml` 加入 crate:

```toml
[dependencies]
kevy-embedded = "*"
```

然后在 `main.rs`:

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

    // 时间点快照。文件落盘后才返回;每个 shard 的锁只在视图冻结
    // 与最终 rename 时短暂持有。
    store.save_snapshot()?;

    // 按需 AOF 压实。锁的使用规则同 save_snapshot。
    let _stats = store.rewrite_aof()?;

    // 实时内省。
    let info = store.info();
    println!("{} keys, {} bytes AOF", info.keys, info.aof_bytes);

    Ok(())
}
```

使用默认配置的全新嵌入式 store 只写 AOF —— 在 `save_snapshot` 运行之前不会出现快照文件。这是预期行为;光靠 AOF 就足以重建键空间。

## 配置旋钮

### 耐久性与 AOF 增长

| 旋钮 | 服务器(TOML / `CONFIG SET`)| 嵌入(`Config::…`)| 默认值 | 备注 |
|---|---|---|---|---|
| AOF fsync 策略 | `appendfsync`(`always` / `everysec` / `no`)| `with_appendfsync(AppendFsync::…)` | `EverySec` | 服务器侧可实时调整。 |
| AOF 是否启用 | `appendonly`(`true` / `false`)| 由 `with_persist(...)` 隐式开启 | `true`(服务器);嵌入式在未调用 `with_persist` 前关闭 | 关闭则跳过所有磁盘持久化。 |
| 自动重写百分比 | `auto_aof_rewrite_percentage` | `with_auto_aof_rewrite(pct, min)` 的第一个参数 | `100` | `0` 关闭自动重写。 |
| 自动重写最小尺寸 | `auto_aof_rewrite_min_size` | `with_auto_aof_rewrite(pct, min)` 的第二个参数 | `67108864`(64 MiB)| 两个阈值都满足才触发自动重写。 |
| 持久化目录 | `dir` / 环境变量 `KEVY_DIR` | `with_persist(path)` | 服务器为 `./data`;嵌入式无默认 | 每个 kevy 实例一个目录。 |
| reactor / reaper 节拍 | reactor tick,约 100 ms | 后台 reaper,或你自己调用 `Store::tick` | 约 100 ms | 驱动 `EverySec` flush、自动重写检测、TTL 清理。 |

### 触发面

| 动作 | 服务器 | 嵌入 | 阻塞形态 |
|---|---|---|---|
| 同步快照 | `SAVE` | `Store::save_snapshot()` | 文件落盘后才返回;锁仅在冻结 + rename 期间持有。 |
| 后台快照 | `BGSAVE` | 在工作线程里调用 `save_snapshot` | 立刻返回;落盘完成后的一个 reactor tick 内提交。 |
| AOF 重写 | `BGREWRITEAOF` | `Store::rewrite_aof()` | 原子 rename 完成后返回;序列化期间键空间仍在线。 |
| 实时调整 fsync | `CONFIG SET appendfsync everysec` | 重建 `Config` | 无 |

### fsync 策略语义

| 策略 | 耐久性 | 代价 |
|---|---|---|
| `Always` | 零丢失 —— 每次写入回复前都 fsync | 吞吐约降 50% |
| `EverySec`(默认)| 崩溃时最多丢失约 1 秒的写入 | 廉价 |
| `No` | 交给 OS 页缓存刷写 | 最廉价 |

## 取舍与限制

**各策略的吞吐 vs 数据丢失。** `Always` 在每次回复前阻塞做 `fsync`,是唯一能在 `kill -9` 下保证零命令丢失的策略,在典型 NVMe 上把以 SET 为主的吞吐砍掉大约一半。`EverySec` 在后台每秒刷一次,崩溃时最多丢失这一秒窗口 —— 默认即此,因为它正好匹配 Redis 的取舍,丢失窗口通常可接受。`No` 让内核决定;吞吐最高,但崩溃可能丢掉所有还停留在页缓存里的写入,可能跨越数秒。

**AOF 重放成本 vs 快照加载成本。** 没有快照时,启动时间随 AOF 字节数线性增长:4 GiB 的 AOF 在本地 NVMe 上可在数秒内回放完毕,40 GiB 则需要一分钟以上。快照能为此设上限 —— 加载是一次流式读 + 一段较短的快照之后的 AOF 尾巴 —— 但要付出短暂的视图冻结代价(O(keys),每个键纳秒级,因为集合类型的值是引用计数共享的)以及对快照落盘期间首次发生改动的集合的一次性 copy。对于写密集型负载,优先靠自动重写把 AOF 控制在合理大小,而不是定期 `BGSAVE`:重写给你同样的启动时间上限,而且不需要管理第二个文件。

**后台任务并发。** 每个 shard 同一时刻最多一个后台保存或重写。在同一任务进行中到达的重复请求会以一条日志记录被跳过,不会进入队列。

**TTL 持久化。** TTL 被写成绝对的 Unix 毫秒截止时间(AOF 里是 `PEXPIREAT`,快照格式里是一个绝对字段),因此键在任意次数的重启之间保留它原本的过期时刻,进程停机的那段时间也会被正确扣除。把相对剩余时间记录到的旧 AOF 仍能加载(载入时视作相对时间);新写入永远是绝对的。`EXPIREAT` 和 `PEXPIREAT` 作为客户端命令对外开放。

**shard 布局变更对崩溃幂等。** 改变 `--threads` / `shards` 会用 `.reshard` 临时名写出新快照,通过一份耐久的 `reshard.journal` 提交,并在下次启动时把中断的迁移向前推进完成。源文件作为 `.premigration.<unix_ts>` 备份保留;journal 是提交点,绝不可手工删除。

**哪些不持久化。** Pub/sub 频道、订阅以及尚未投递的消息只存在于内存里。`BLPOP` 这类阻塞命令的等待者以及阻塞 `XREAD` 是连接状态,不是数据。两者既不会写入 AOF 或快照,也不会被回放。

## FAQ

### 我的 AOF 文件在不断增长 —— 如何压实?

在服务器上执行 `BGREWRITEAOF`,或在嵌入式模式下调用 `Store::rewrite_aof()`。重写会以重建当前键空间所需的最小命令集来重写日志 —— 每个键一条 `SET` / `HSET` 等,加上 TTL 键的 `PEXPIREAT` —— 然后原子地把新文件换入。一万次对 `hot` 的覆盖会压缩成一条 `SET hot <latest>`。

对于无人值守运维,把自动重写保持默认值即可 —— 相对上次重写增长 100% 且至少 64 MiB —— reactor 会自己触发压实。设 `auto_aof_rewrite_percentage = 0` 可禁用,完全由人工驱动重写。

重写对键空间是非阻塞的:序列化和 `fsync` 期间读写仍在流动,重写过程中落地的任何写入会被 tee 到一个 diff 缓冲区,最终追加到压实后的镜像中。如果重写在中途崩溃,原 AOF 没被动过(替换是一次原子 `rename`),残留的 `aof-<id>.aof.rewrite` 临时文件可以安全删除。

### 我能完全关闭持久化吗?

可以,两种方式:

- **服务器:** 在 `kevy.toml` 设 `appendonly = false`(或省略 `--dir`)。服务器作为纯内存缓存运行;不创建任何 `aof-*` 或 `dump-*` 文件。
- **嵌入式:** 构造 `Config` 时不调用 `with_persist(...)`。`Store::open` 让整个键空间留在内存中;`save_snapshot` 和 `rewrite_aof` 在 API 层面成为无操作(或返回一个表明未配置持久化目录的错误)。

如果你想要持久化但又希望在两次快照之间 AOF 完全不增长,这种组合不被支持 —— kevy 的耐久模型是 AOF-first,快照存在的目的是限定 AOF 重放,不是替代 AOF。

### 高写入负载下做一次快照的代价是多少?

阻塞的部分非常小。每个 shard 对键空间的冻结是 O(keys),而非 O(bytes),因为集合类型的值是引用计数的,与实时 store 共享;百万级键的 shard 冻结只需个位数毫秒。序列化本身是在键空间在线的情况下进行的 —— 写入并不会被暂停。

你要付出的暂时代价是内存。在快照写出过程中发生过改动的任何集合(list / hash / set / sorted-set)都会被克隆一次,这样实时 store 才能继续推进而不打扰冻结视图。以 `SET` 普通字符串键为主的负载下,额外内存可以忽略;以对少量巨大集合做 `HSET` / `LPUSH` 为主的负载下,这些特定集合的常驻大小可能短暂翻倍。

一次成功的快照也会重置 AOF —— 快照现在承载了原本日志里的全部内容,日志从冻结之后落地的写入重新起步。下次启动时加载快照 + 日志,不会重复施加历史。

### 下一次启动时如何对恢复进行排序?

对每个 shard,依序:

1. **加载快照。** 如果 `dump-<id>.rdb` 存在,流式载入键空间。已过期 TTL 在加载阶段被丢弃。
2. **回放 AOF。** 从 `aof-<id>.aof` 头部开始,依次施加每一帧。
3. **处理尾部。** 干净文件完整施加。截断的尾部(写到一半崩溃)丢弃残缺的尾帧,施加前缀。损坏的帧会被移到 `aof-<id>.aof.panic-quarantine.<unix_ts>` 一边,避免阻塞后续启动,然后施加前缀。隔离的尾部不会被重新施加;如果需要从中恢复任何内容请手工检查。
4. **打印一条单行摘要**,包含挂钟耗时:

   ```text
   kevy: AOF /data/kevy/aof-0.aof replayed 145313 commands from 418261733 bytes in 247 ms (clean)
   ```

5. **通过回放 `reshard.journal`** 把任何被打断的 shard 布局迁移向前推进。

关注重放耗时这一行,并依靠自动重写把它控制在范围内 —— 重放时间随未重写 AOF 大小线性增长。

### 在嵌入式宿主进程内部如何监控持久化?

两个接口面。

**轮询。** `store.info()` 返回 `KevyInfo` 结构,包含 `keys`、`used_memory`、`aof_bytes`、`expire_pending`、`evictions`、`expired_keys`。更细粒度的辅助方法覆盖同样的信息:

```rust
store.dbsize();                 // 实时键数量
store.ttl(key);                 // Option<Duration>(None = 无键 / 无 TTL)
store.ttl_ms(key);              // Redis PTTL 语义:-2 无键、-1 无 TTL,否则为毫秒
store.expire_pending_count();   // 带 TTL 的实时键数
store.used_memory();            // 常驻字节估算
store.expired_keys_total();     // 累计过期数(惰性 + reaper)
store.evictions_total();        // 因 maxmemory 累计驱逐数
```

如果你期望存在 TTL 时 `expire_pending_count() == 0`,那就是 TTL 子系统没登记到你的键的经典征兆。

**推送。** 用 `Config::with_metric_sink(...)` 注册,即可在 AOF 重放(启动)与每次 AOF 重写(压实)时收到 `KevyMetric` 事件。sink 同步运行在事件发出的线程上(后台重写跑在 reaper 线程),保持回调短小。`KevyMetric` 是 `#[non_exhaustive]` —— 始终匹配一个 `_` 分支以保持向前兼容。

### 持久化目录里每个文件是什么?

| 模式 | 含义 |
|---|---|
| `aof-<id>.aof` | shard `<id>` 的实时 AOF。 |
| `dump-<id>.rdb` | shard `<id>` 的二进制快照。 |
| `shards.meta` | 已记录的 shard 数与路由方案。 |
| `dump-<id>.rdb.tmp` | 进行中的快照写出。陈旧的可安全删除。 |
| `aof-<id>.aof.rewrite` | 进行中的 AOF 重写/重置。陈旧的可安全删除。 |
| `dump-<id>.rdb.reshard` + `reshard.journal` | 进行中的 shard 布局迁移。下次启动会向前推进;绝不可手工删除 journal。 |
| `*.premigration.<unix_ts>` | 迁移前的源文件备份,留待回滚。 |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | 恢复期间被搁置的 AOF 损坏尾部。需要抢救任何内容时请手工检查;kevy 不会重新施加它。 |
