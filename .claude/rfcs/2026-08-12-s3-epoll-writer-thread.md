# RFC: S3 — epoll/kqueue 反应堆的 AOF writer 线程(post-v5)

Status: DESIGN(设计轮 2026-08-12;地面真相 = Explore 全图,本文 file:line 均实查)
上游:`.claude/rfcs/2026-08-08-v5-v3-aof-offload.md` B 案(writer 线程先例拍板)
     `.claude/rfcs/2026-08-12-s2-always-cqe-gated.md` §4(epoll 边界=S3 另立弧)

## 1. 现状与目标

epoll 与 kqueue **完全共路径**(shard_run.rs:67 一条 `Shard::run`;
kevy-sys 双 Poller 同 API;uring_* 全部 cfg(linux))。今天这条路径:

- append 同步写 BufWriter(exec.rs:308 → aof.rs:312,256KiB 缓冲);
- everysec 在主循环 maybe_sync(shard_run.rs:311,反应堆上同步
  flush+sync_data);
- always 在每批 end_group 同步 fsync(aof_txn.rs:126,反应堆阻塞
  一整个 fsync 延迟),之后才 flush_conn(inbox.rs:88 → :102)。

> **目标不变量(与 S2 同)**:任何写命令的回复字节离开本机前,该写
> 已持久。
> **收益目标**:epoll/kqueue 反应堆不再为 write/fsync 阻塞;always
> 下多连接组提交;S5 off-thread swap 对 epoll **按构造解锁**
> (persist_rewrite.rs:150 判 `queued_mode()`,非反应堆种类)。
> 爆炸半径注意:workspace 全部集成测试强制 KEVY_IO_URING=0
> (uringgate.sh:8-11)——PR 测试矩阵就是 epoll 矩阵;macOS 开发面
> 即 kqueue 主路径。

## 2. 设计

### 机制:per-shard writer 线程 + 复用 queued-append

- **复用 S1 的 queue 面**(kevy-persist 零改语义):
  `enable_queued_appends()`;shard tick `take_pending()` 只取
  bytes——AOF fd 一律 O_APPEND(aof.rs:193 等四处),offset 是记账
  值,顺序 `write_all` 与 uring positioned write 磁盘字节等价。
  S2 的 queued+Always 陷阱修与 `queued_seq` 水位线原样生效。
- **kevy-persist 补一个安全句柄出口**:
  `queued_file_clone() -> io::Result<File>`(`try_clone`,纯 std,
  不破 forbid(unsafe_code));writer 线程持 clone,fd 生命周期与
  rewrite 重开的交互见「结构性操作」。
- **新文件 `crates/kevy-rt/src/aof_writer.rs`**(inbox/persist_worker
  已到 500 LOC 线):`AofWriterLane`,persist_worker/bio 先例形状——
  - 每 shard 一条线程 `kevy-aofw-{id}`,懒启动;std mpsc 保序。
  - Job:`Append(Vec<u8>)` / `Fsync { covers: u64 }` /
    `Reopen(File)`(rewrite swap 后换句柄)。
  - Done 回程(第二 channel):`Appended(Vec<u8>)`(缓冲归还复用)/
    `Synced { covers }` / 错误报告;完成后 `waker.wake()`
    (Shard.waker 已注册 poller,waker.rs Send+Sync)。
  - shard 侧记账:`submitted_appends` vs `completed_appends`
    (restructure 谓词用)+ `durable_watermark`(与 uring_aof 同名
    同义)+ `fsync_inflight`。
  - **dead-thread 兜底**:submit 失败按 `submit_reclaim_tail`
    先例(persist_worker.rs:125-140)拿回缓冲走同步路径,响亮日志。
- **fsync 调度归 shard 侧节奏**(与 uring_aof maybe_fsync 对称):
  - everysec:既有 1s 节奏,队列+在途排空时 submit
    `Fsync{covers=watermark}`;
  - always:队列+在途排空且 `watermark > durable` 即刻 submit——
    组提交与 S2 同构;
  - `Synced{covers}` 回程推进 `durable_watermark`,冲刷
    held_responses,waker 唤醒后 flush 重扫。
- **always 回复持有(S2 三件套的 epoll 对偶)**:
  - `Conn.held_watermark: Option<u64>`(conn.rs #[repr(C)] 布局注释
    要看——加在冷区尾部);
  - `always_hold_w0()` 两个 cfg 版本改为看「lane 启用 + Always」
    (shard_flush.rs:233-252,linux 版加 lane 分支,non-linux 版从
    硬编码 None 变真实现);
  - stamp 点 = dispatch 批后(inbox.rs:88 之后 :102 之前)+
    RequestBatch(w0 已在,S2 加过);
  - **门点 = flush_conn 写循环入口一处**(shard_flush.rs:187,覆盖
    全部 7 个调用点):held > durable → 本次不写,`want_write` 保持
    /pending 重扫;
  - `held_responses` 字段与 `send_or_hold_response` 的 cfg(linux)
    放宽为全平台;`mark_all_durable`(swap 完成)同。
- **结构性操作**(rewrite begin/finish、truncate、SAVE):
  - epoll 版 restructure 谓词 = 队列空 && 在途空(对
    uring_aof.rs:293-297);
  - swap hold:`swap_holding()` 时 shard 停止 take_pending/submit
    (对 uring_aof.rs:168-170),writer 排空即安全;
  - swap 完成:`Reopen(new_file_clone)` 交 lane + mark_all_durable
    (对 persist_rewrite.rs:258-259,cfg 放宽);
  - `flush_queued()` 同步兜底仍是结构性入口的诚实回退(在途必须先
    排空,谓词保证)。
- **默认态**:与 uring 同一开关 `KEVY_AOF_OFFLOAD`(=0 退回经典同步
  路径);默认 ON——PR 矩阵即 epoll,测试覆盖是优势不是风险。
- **embedded 不动**(无反应堆);uring 路径零改动。

### 已知边界(记录即接受)

- held 字节计入 `enforce_output_limit`(shard_tick.rs:213):慢盘下
  持有累积可触发保护性断连——宁断不假 ack,与 S2/uring 语义一致。
- `resolve_serve_by_write`(shard_flush.rs:213)随门推迟:escrow
  释放本就应绑定「回复真离开」,语义正确;实施时复核 C1 测试。
- 回复延迟 ≈ fsync 延迟 + 一次 waker 往返(单序贯写手的 S2 同款
  代价;并发即组提交)。

## 3. 验证面(先行——现状零专属门)

Explore 确认:crashgate cell S / tailgate / crash-test.sh 全部 auto
反应堆(盒上/CI = uring;本机 = kqueue);perfgate 强制 uring 且
--no-aof。**S3 在 Linux 上没有任何显式 epoll 门**。

1. **新 crashgate cell:server-always-epoll**(实施第一步):cell S
   的对偶,显式 `KEVY_IO_URING=0`,旧同步路径先跑绿作基线(CI
   ubuntu + 盒),S3 落地后必须同绿。
2. kevy-persist queued 单测三件套(tests_aof.rs:570-663)= 复用
   queue 的现成回归网;`queued_file_clone` 加单测。
3. aof_writer.rs 单测:保序、缓冲归还、fsync covers 推进、dead-
   thread 兜底。
4. 盒上 perf A/B:`KEVY_IO_URING=0` + always,c50/c1(对照 S2 的
   353→8273 形状);everysec 吞吐无回归(A/B offload=0 vs 默认)。
5. workspace 全测(即 epoll 矩阵)+ crashgate 全矩阵 + repligate +
   availgate + perfgate-median(uring 面零改动的回归证明)+ CI。
6. docs/persistence.md:217 的 epoll 语义句子改写。

## 4. 不做 / 边界

- 不改 uring 路径一行(S2 机制原样)。
- 不做有界 SPSC 环(原 B 案):std mpsc 无界 + tick 级 take_pending
  批量化已够;背压由 enforce_output_limit + 客户端 TCP 背压承担。
  环满=阻塞的拍板②随之作废(无环)。
- 复制面/选举面不动。

## 5. 实施顺序

1. 验证面先行:crashgate server-always-epoll cell,旧路径基线绿。
2. kevy-persist:`queued_file_clone()` + 单测。
3. `aof_writer.rs`:lane 全套 + 单测(不接线,dead code allow 或
   同 commit 接线择一——按 S2 经验直接④连提)。
4. 接线:setup/tick/restructure/swap-hold/Reopen + always 门
   (Conn.held_watermark + flush_conn 谓词 + cfg 放宽)。
5. 盒上 cell + perf A/B + workspace/crashgate/tailgate 观察跑 +
   perfgate-median + 全门禁 → merge。

## 6. Resolution(实施轮,2026-08-12,feature/s3-epoll-writer-thread)

按 §5 顺序落地,三处实施期收紧:

1. **dead-lane 回收**:submit 失败时 SendError 携带的 chunk 通过新
   `Aof::requeue_front`(kevy-persist)回插队首——offset 记账回退,
   同步兜底 flush_queued 恰好一次落盘、顺序保持(回归测试钉死)。
2. **fsync-policy switch 的交错缝**:CONFIG SET appendfsync 的
   set_fsync 会经 OWNER 句柄 flush_queued;lane 在途时两句柄交错会
   乱序。apply_live_persist_knobs 先 `epoll_aof_settle()`(有界
   自旋 reap 到排空)再应用。**注:uring 侧 live-config 切换若可达
   同类缝(positioned write vs owner O_APPEND),属存量面,单列
   后续审计项,不在本弧。**
3. **门点防自旋**:park 时撤 write interest(level-triggered poller
   否则会对"可写但拒写"的 socket 热转),释放由 lane 唤醒后对
   held_conns 直接 flush_conn。

S2 三件套全平台化:always_hold_w0 / held_responses /
send_or_hold_response / flush_held_responses 摘除 cfg(linux)
(uring_flush_held_responses 删除,双驱动共用 portable 版,双水位
取 max——单 shard 只有一个驱动活跃,另一个恒 0)。

验证数字见 bench/FINDING-2026-08-12-s3-epoll-writer-lane.md
(盒上 epoll always c50 +22×,c1 持平且 fsync-bound=门无泄漏)。
