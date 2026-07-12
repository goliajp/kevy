# CDC——变更流（`FEED.*` / `changes_since`）

kevy 把每一笔已应用的写入都作为可消费的变更流暴露出来——就是 AOF 记录、副本应用的那一批 effect 帧。典型消费者：缓存失效器、搜索索引更新器、下游镜像——也就是 RDS 栈里那张“outbox 表”干的活，只是不需要那张表。

在配置里打开：

```toml
[feed]
enabled = true
# feed_buffer_size = "64mb"   # per shard, caps at 1gb
```

embedded 侧则是 `Config::default().with_feed(0)`。

## 游标：`(generation, offset)`

每一个流位置都是一对 `(generation, offset)`：

- `offset` 在一个 generation 内，每应用一笔写入就加一。
- `generation` 标识一段从未断裂的 offset 历史。给定的 `(generation, offset)` **永远**指向同一段流前缀。
- **干净关停 + 重启会同时保住两者**——消费者从上次停下的地方继续。
- 连续性一旦断裂，generation 递增、offset 从 0 重开：`FLUSHALL`、从快照恢复，或者一次**不干净的关停**（崩溃）。generation 变了，就是在告诉消费者“你的视图可能已经陈旧”：先重建，再续读。

## 服务器接口面

- `FEED.SHARDS` → `:N`——独立流的条数（每个 shard 一条；键映射到 shard，所以**逐键顺序在一条流内有保证**；跨 shard 没有顺序）。
- `FEED.TAIL <shard>` → `*2 [:generation, :next_offset]`——新消费者的起始游标。
- `FEED.READ <shard> <gen> <offset> [COUNT n] [PREFIX p ...]` → `*3 [:generation, :next_cursor, *frames]`，每一帧是 `*2 [:offset, *argv]`。轮询它；帧列表为空表示已追平。
  - `COUNT` 给单次调用的帧数封顶（默认 256，最大 4096）。
  - `PREFIX`（可重复，OR 关系）按键前缀过滤帧，**fail-open**：键布局不是普通单键形态的帧（多键 `DEL`、`MSET`、`FLUSHALL`……）总是会被投递。过滤永远不改变游标——你可以随时换过滤条件，不必重新同步。

### 错误

- `-FEEDRESYNC <gen> <tail>`——你的游标已经无法服务（generation 太老，或者那段 offset 已被挤出 backlog）。先重建你的派生状态（例如 `SCAN` 一遍你的前缀），再从回复里带回来的 `(gen, tail)` 续读。
- `-ERR feed cursor ahead of stream`——来自未来的游标；调用方的 bug。

## Embedded 接口面

```rust
let store = kevy_embedded::Store::open(Config::default().with_feed(0))?;
let (gen, off) = store.changes_tail()?;             // start cursor
let batch = store.changes_since(gen, off, 256, &[b"user:"])?;
for change in &batch.changes { /* change.offset, change.argv */ }
let (gen, off) = batch.next;                        // resume here
```

`feed_shards()` 恒为 1——embedded 的写路径把所有 shard 串成一条流，所以同一份消费者循环对两个接口面都成立。`FeedError::Resync { generation, tail }` 对应 `-FEEDRESYNC`。

## 投递语义

**At-least-once。** 在一次 resync 重建之后（以及某些切主形态下），你可能看到已经应用过的帧——消费者必须幂等（缓存失效天然是幂等的）。帧携带的是已应用的 **effect**（例如一次 `ZINTERSTORE` 会以 `DEL` + 普通 `ZADD` 的形式出现），所以不管源状态如何，应用这些帧都是确定性的。

**保留窗口**就是内存里的 backlog（每 shard 一份 `feed_buffer_size`）：落后超出这个窗口，你就会拿到 `-FEEDRESYNC`。没有磁盘归档——耐久的那一半故事，见下面的恢复点契约。

## 恢复点契约（PITR）

开着 feed 时拍的快照，会把流游标记进头部，并且和快照数据冻结在同一个 no-append 窗口里：

> **快照 S + 从 S 的游标起的 feed 帧 = 之后任意游标处的精确状态。**

`kevy_persist::read_snapshot_cursor(path)` 把它读回来（v2.3 之前的快照返回 `None`）。一次恢复演练：把快照载入一个全新的数据目录，然后从记录下来的游标开始重放 `FEED.READ` 的帧——得到逐字节精确的状态，可以用 `PREFIX.STATS` / 键 dump 验证。演练脚本就是 `bench/diskgate.sh` 里的 restore 那一行。

## 逐前缀统计

`PREFIX.STATS <prefix>`（服务器）/ `Store::info_prefix`（embedded）报告某个字节前缀下的存活键数与带 TTL 的键数——这是 O(键空间) 的遍历，给运维看板用，不给热路径用。

## 这不是什么

没有跨 shard 的全局顺序，没有服务端保存的消费位点，没有 consumer group，没有查询谓词（前缀只是一个字节过滤器）。如果你需要这些，你描述的是一个消息中间件或者一个 RDS——kevy 的 feed 是刻意停在视界之前的（见 [designing-on-kevy.md](designing-on-kevy.md) 里的三条法则）。
