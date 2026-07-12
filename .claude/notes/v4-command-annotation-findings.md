# v4 命令参考标注 —— 顺带挖出的真问题(2026-07-12)

给 189 个命令标注 `complexity` + `redis_compat` 时,五路 agent 逐个读实现,
挖出一批**必须在命令参考上如实公开**的兼容性事实,以及若干真代码 bug 与文档 bug。

## A. 必须公开的兼容性差异(读者迁移时会踩)

| 命令 | 事实 | 证据 |
|---|---|---|
| **SCAN** | **不是游标迭代器。** 每次调用做全量跨 shard 键空间扫描,cursor 永远回 `0`,`COUNT` 接受并忽略,`TYPE` 静默忽略,`MATCH` 生效。标准 SCAN 循环一轮即终止。Redis 的增量/有界/容忍 rehash 保证**全部不存在**。 | `cmd_resolve.rs:149`、`cmd.rs:330-339`、`exec_build.rs:139-143,315-331`、`reduce.rs:140-148`(硬编码 `"0"`) |
| **RANDOMKEY** | **不随机。** 每 shard 返回桶序第一个活键,reducer 取第一个 shard 的第一个 → 固定键空间下每次返回同一个键。**不能用于采样。** | `exec_build.rs:139-143`、`keyspace.rs:299-317`、`map/iter.rs:68-74`(`iter()` 从 bucket 0 起,随机化的 `iter_from_bucket` **未被使用**)、`reduce.rs:149-152` |
| **MSET** | **只在单 shard 内原子。** 按键分组后逐 shard 应用,无跨 shard 屏障 → 并发读者可能看到一部分已写、一部分未写。Redis 的 MSET 是全局原子的。 | `exec_build.rs:195-215`、`exec_op.rs:93-108` |
| **MGET / EXISTS / DEL / UNLINK** | 同上:多键形式跨 shard 分组、无屏障 → 不是时间点快照 / 不是原子删除。 | `exec_build.rs:218-245,334-351` |
| **RENAME / RENAMENX** | **跨 shard 时是非原子的 Take→Put** —— 存在一个源键已消失、目标键未出现的窗口。RENAMENX 被拒时还要再 Put 回去。同 shard(用 `{hashtag}` 共置)才原子。 | `exec_rename.rs:41-116,156-240` |
| **UNLINK** | **是 DEL 的别名。** 值在 reactor 线程就地释放,无后台回收 → **零延迟收益**。 | `dispatch.rs:405-417` |
| **SET** | 只解析 `NX/XX/EX/PX`。**`KEEPTTL` / `EXAT` / `PXAT` / `GET` / `IFEQ` 系列全部返回 syntax error。** 且重复的 expire 选项被静默接受(last-one-wins),不报错。 | `cmd_data.rs:171-210` |
| **EXPIRE / EXPIREAT / PEXPIRE / PEXPIREAT** | **不支持 `NX/XX/GT/LT`** —— arity 固定为 3,带条件标志直接 `wrong number of arguments`。 | `cmd_data.rs:274-324`、`verb_meta.rs:100-104` |
| **APPEND** | 值 ≤ 64 B(`Value::Str`)时摊还 O(1);**一旦 > 64 B 变成 `Value::ArcBulk`,每次 APPEND 都是 O(N)** —— 反复追加构建大字符串是 **O(N²)**。 | `string_rmw.rs:15-64`(ArcBulk 臂 `a.as_ref().to_vec()`)、`value.rs:197` |
| **INCRBYFLOAT** | f64 运算 + Rust 最短往返打印,**尾数可能与 Redis 的 long double 发散**。compat3 只测了 `3.0` / `1.5` 两个精确值,**没测到这个差异**。 | `string_rmw.rs:119-181`、`util.rs:224-241` |

**compat3 的覆盖盲区**:`KEYS` / `SCAN` / `RANDOMKEY` / `UNLINK` / `EXPIREAT` / `PEXPIREAT` / `PTTL` **完全没有差分测试** ——
而这几个恰恰是差异最大的。且 compat3 只跑单 kevy 容器,**上面所有跨 shard 非原子性一条都没被测到**。

## B. 真代码 bug

1. **`IDX.VERIFY` 有一段做完就扔的 O(N) 工作。**
   `cmd_index_query/query.rs:88-102`:`each_entry` 把 segment 里**每一个 entry 克隆进一个 Vec**,然后
   `let _ = (…, entries, spec);` 直接丢掉。O(N) 时间 + O(N) 分配,产出为零。
   它号称要算的 `drift` 统计**根本不在返回值里**(`cmd_index_reduce/query.rs:243-251` 只回
   entries/bytes/coerce_failures/duplicates)—— 而 `verb_meta.rs:253` 和 `docs/verb-reference.md:264`
   都还在宣传 drift。要么把 drift 做完,要么把死代码和文档一起删。

2. **`ReplicationSource::frames_from` 的注释说自己二分,实际线性扫描。**
   `kevy-replicate/src/source.rs:212-214` 注释:*"binary search is correct; the deque slices into two parts
   so we iterate"* —— 然后调用 `self.buf.iter().position(...)`,O(B)。`VecDeque::as_slices()` +
   两半各做 `partition_point` 即可 O(log B)。**在 FEED.READ 和 replica 流两条路径上。**

3. **`COMPOSE` 无视 `LIMIT`。**
   `cmd_index_query/ops.rs:236-260`:两个 leaf 都用 `usize::MAX` 全量物化 → 排序 → 去重 → **最后才截断**。
   `COMPOSE OR` 两个宽范围 + `LIMIT 10`,每一页、每个 shard 都要付两个完整范围 + 一次排序。
   扩展面里其他形状全是 cursor-paged + limit-bounded,只有这个例外。

## C. 文档 bug

- `docs/migration.md:175-179` 称 SCAN 是 *"a full incremental walk"* —— **是全量非增量扫描**。
- `docs/verb-reference.md:59` + `verb_meta.rs:96` 称 SCAN *"Incrementally iterate"* —— 同上;且 TYPE 也是"接受并忽略"。
- `verb_meta.rs:253` + `docs/verb-reference.md:264` 称 IDX.VERIFY 报 **drift** —— 返回值里没有。
- `docs/views.md:126-127` 称 VIEW.VERIFY 让 drift 可证伪(members/bytes/order-exclusions)——
  **virtual view 的 bytes 和 order-exclusions 硬编码为 0**,且 VIEW.VERIFY 在 virtual view 上要付一次
  **完整的 `eval_tree`**,恰与 `docs/views.md:70-74` 卖的 "O(limit/selectivity) 而非 O(members)" 相反。
- `docs/views.md:91` 称 "O(1) rejection of non-contenders" —— 实际是 `BTreeSet` 的
  `iter().next()/next_back()`,**O(log members)**。(同文件 `:85` 的 "O(log members) per affected write" 是对的。)
- `docs/vector-search.md:127` 称 IDX.REBUILD 是 "bounded O(n·EF)" —— 漏了 `M`(邻居上限,可达 64)
  和 `dim`(可达 65536)两个因子,真实是 `O(n · ef_construction · M · dim)`。

## 处置

- A 类 → **全部进命令参考的 `Compatibility with Redis` 栏**(这一栏是 Redis-compatible 引擎最该有的东西,
  Redis 自己的文档都没有)。同时补 `docs/migration.md` 的"迁移时会咬你的地方"一节。
- B 类 → 真 bug,修。
- C 类 → 文档改成如实。

---

## 处置结果(2026-07-12)

### 已修的真 bug(全部为跨 shard 路由类,且全部在默认配置下触发)

| bug | 症状 | 实测 | 修法 |
|---|---|---|---|
| **RPOPLPUSH / LMOVE 跨 shard 丢数据** | 命令**返回被移动的值**,但目标列表是空的 —— 值消失 | 8 shard,**12 对键丢 11 对** | 新 `Route::ListMove` + `exec_listmove` 的 Take→Push→Restore 编排;同 shard 仍走原子路径 |
| **BRPOPLPUSH 跨 shard 丢数据** | 同上,且阻塞唤醒后也丢 | 8 shard,**12 对键丢 9 对** | 目标远端时不许跑本地 dispatch,强制走跨 shard 仲裁;仲裁的 serve 改跑编排器(blocking 模式:命中→回复+解挂,竞争落空→重新挂起) |
| **GEOSEARCHSTORE / GEORADIUS STORE 跨 shard 写错 shard** | `:0` 或 `-ERR could not decode requested zset member` | 6 对目标**5 个失败** | 新 `Route::GeoStore { src, dst }` + `exec_geostore`,复用 `ZAlgebraStore` 的目标端写入 |
| **STOREDIST 无视单位** | `km` 查询存 `166274.15`,Redis 存 `166.27` | — | 按查询单位换算 |
| **GEOSEARCH DESC + COUNT 返回最近的 N 个** | 恰好是相反的结果集 | — | 截断前的升序重排改为仅在 `Sort::None` 时生效 |
| **MEMORY USAGE 路由错** | 按子命令名 `"USAGE"` 哈希 → 键不在那个 shard 就返回 nil | — | `MEMORY USAGE` 按 args[2](键)路由 |
| **MEMORY STATS 只报一个 shard** | operator 拿它来给机器定容量 | — | 改读实例级聚合 gauge(与 `INFO memory` 同源) |

**这一类 bug 为什么全都活到了 v4**:每一个 geo / list 的集成测试都用 `Server::start(1)` —— **单 shard 把整类 bug 完全掩盖**。
`bench/compat3.sh` 也只跑单实例,所以差分测试同样看不见。新加的 9 个 geo 测试全部跑在 `Server::start(8)` 上。

### 还未处置

- `IDX.VERIFY` 的 O(N) 死代码 + 不存在的 `drift` 统计(见 B 节)
- `ReplicationSource::frames_from` 注释称二分实为线性(见 B 节)
- `COMPOSE` 无视 `LIMIT`(见 B 节)
- A 节的全部兼容性事实 → 进命令参考的 `Compatibility with Redis` 栏
- C 节的文档 bug → 改成如实
