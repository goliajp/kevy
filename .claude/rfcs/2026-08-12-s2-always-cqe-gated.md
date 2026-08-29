# RFC: S2 — appendfsync=always with CQE-gated replies(post-v5)

Status: DESIGN(设计轮 2026-08-12;地面真相 = Explore 全图,本文 file:line 均实查)
上游:`.claude/plans/2026-08-10-v5-final-runway.md` §已勾账 post-v5(S2 条)

## 1. 现状与目标不变量

今天 uring 反应堆的 always:每个 recv CQE 一个 fsync 窗
(uring_io.rs:254 begin → 执行/append(deferred,dirty)→
uring_io.rs:259 end_group = **反应堆上阻塞 flush+sync_data**
(aof_txn.rs:120-128)→ 下一迭代顶部回复才进 write SQE
(uring_reactor.rs:159))。语义正确,代价是反应堆按 fsync 延迟阻塞。
offload 明确拒绝 always(uring_aof.rs:90-97,S1 范围)。

> **目标不变量(不变)**:任何写命令的回复字节离开本机之前,
> 该写已持久(fsync 完成)。
> **收益目标**:反应堆不再为 fsync 阻塞;fsync 由 ring CQE 驱动,
> 多连接写在同一 fsync 轮里天然组提交(优于今天 per-recv-CQE 的
> fsync 频率)。

## 2. 设计

### 机制:epoch 门控的回复持有

- **打开 offload under Always**(去掉 uring_aof.rs:90 守卫),
  append 走 queue(aof.rs:294-303)。
- **陷阱必修**(Explore 抓出的潜在正确性坑):queued 模式下
  aof.rs:323-326 与 aof_txn.rs:122-127 的同步 fsync 对着空文件
  sync——bytes 还在 queue 里。Always+queued 分支一律只置
  dirty,真正的持久由 ring fsync 负责。
- **epoch**:复用 InflightChunk.seq 的单调计数(uring_aof.rs:35,63)。
  定义 `append_epoch` = 最近一次入 queue 时的下一 fsync 轮编号;
  `durable_epoch` = 最近成功 fsync CQE 覆盖到的轮。
- **fsync 调度(不需要 SQE 链接)**:沿用今天的排空后 fsync 纪律
  (uring_aof.rs:158-166:仅当 inflight 空才提交 fsync)——Always
  下把「每秒」触发条件换成「有脏字节且有被持有的回复即刻触发」:
  每 tick 的 uring_aof_tick 里,queue 排空为 chunk → chunk 全完成
  → 立即 prep_fsync(新 OP 位:`OP_AOF_FSYNC`,低位带轮编号——
  今天 seq==0 当 fsync 标志的方案与带轮冲突,op 空间 11-15 可用,
  uring_ops.rs:28,56)。
- **回复持有**:Conn 侧不动;UringConn 加
  `held_until_epoch: Option<u64>`(uring_conn.rs,余量 243)。
  执行期间(dispatch 后,uring_io.rs:286 mark_arm_pending 前)
  若本批含写且 policy=Always+offload:标记
  `held_until_epoch = current_epoch`。
  **门点** = uring_arm.rs:185-193 的 swap 前一条谓词:
  `held_until_epoch > durable_epoch` → 跳过本轮 arm(conn 留在
  arm 队列,needs_more 机制天然重试,uring_arm.rs:432-441)。
  fsync CQE 成功 → durable_epoch = 该轮 → 下一 tick 顶部 arm 自然
  放行(回复延迟 ≈ fsync 延迟 + ≤1 tick,反应堆全程不阻塞)。
- **跨 shard 回复**(inbox.rs:297-310 RequestBatch):response
  batch 的 send_to 同样必须持有到 fsync CQE——加一个按 epoch 挂起
  的 `held_responses: Vec<(epoch, origin, batch)>`,fsync CQE 后
  冲刷。面小(一处 send 点)。
- **无回复的 append 面**(drain_tick_frames / feed 等直击
  aof.rs:323 的路径):queued+Always 下只置 dirty,由同一 fsync
  轮收敛;无可持有物,无需门控。
- **fsync CQE 失败**:违反不变量的唯一路径。处置(推荐,实现时
  可再议):响亮日志 + 保持持有 + 立即重试一次;再失败 → 断开被
  持有的连接(宁可断连不可假确认),并 anchor 进 INFO 计数。
- **epoll / embedded 不动**:epoll 保持同步语义(S3 另议);
  embedded 的 always 本就在调用方线程,无反应堆可阻塞。

### 宿主与 LOC

- 主逻辑进 `uring_aof.rs`(267 行,余 233)+ 新常量进
  `uring_ops.rs`;UringConn 加字段;uring_arm.rs 只加一条谓词
  (450 行,加 ~5 行可容);aof.rs 的 policy switch 就地小改
  (433 行,余 67);uring_reactor.rs(493)不加行——新调用走
  uring_aof_tick 既有入口。
- kevy-uring 无需新增 IOSQE_IO_LINK(排空后 fsync 免链接);若
  后续要 write+fsync 单轮合并再议(layout.rs:57 flags 可设,
  prep.rs:190 有 post-construction 改 flags 先例)。

## 3. 验证面(必须先补——现状是零)

Explore 确认:crashgate 的两个 always cell 全是 kevy-embedded
写手(crash_writer.rs:27),**从不经过 uring 反应堆**;server 路径
的 always 覆盖只有 chaos harness 默认值与手动 crash-test.sh。

- **新 crashgate cell:server-always**(实现的第一步,先于改动):
  真 kevy 二进制 + TOML appendfsync=always + KEVY_IO_URING=1,
  客户端逐条写并记录「已收到回复的最大序号」,kill -9,重启断言
  **回复过的全部存活(loss-bound = 0)**——这是不变量的直接机器
  检验,改动前先在旧同步路径上跑绿(基线),改动后必须同绿。
- crash-test.sh 的 `appendfsync=always must lose zero keys` 断言
  (bench/crash-test.sh:116-118)作为第二证据面。
- perf A/B(盒):redis-benchmark SET,appendfsync=always,
  uring,改动前后吞吐/延迟——预期吞吐大幅升(反应堆不再按
  fsync 串行),单op延迟仍 ≈ fsync 延迟(语义使然)。
- perfgate-median 全 cell 无回归(默认 everysec 路径逻辑上零改动
  ——fsync 触发条件重构须证明 everysec 行为逐字节不变)。
- crashgate 全矩阵 + repligate + 全门禁照常。

## 4. 不做 / 边界

- epoll 的 always 保持同步(旧内核 fallback;S3 的 writer 线程
  另立弧)。
- 不做 write→fsync 的 SQE 链接(排空后 fsync 已保序;链接优化
  留观察项)。
- 不改变 everysec / no 的任何行为(重构必须保证)。
- 复制面(replica ack 与 always 的耦合)不在本弧。

## 5. 实施顺序

1. 验证面先行:server-always crashgate cell,旧路径跑绿作基线。
2. kevy-persist:queued+Always 的 append/end_group 分支(只置
   dirty)+ 单元测试(含「同步 fsync 对空文件」陷阱的回归)。
3. uring_aof:epoch 机制 + Always 的 fsync 调度 + OP_AOF_FSYNC。
4. 门点 + 持有字段 + 跨 shard held_responses。
5. 盒上 crashgate cell + perf A/B + perfgate + 全门禁 → merge。

## 6. Resolution(实现轮,2026-08-12,feature/s2-crashgate-server-always)

实现相对 §2 的三处改道(均为简化,不变量不动):

1. **epoch 轮 → 记录水位线**。kevy-persist 加单调
   `queued_seq`(`queued_watermark()`,rewrite swap 不重置——文件
   offset 会重置,曾是 wedge 隐患),offload 侧 `durable_watermark`
   / `fsync_covers`。逐记录精确,且**不需要 OP_AOF_FSYNC 新 tag**:
   fsync 单飞行(`fsync_inflight`)使 `seq==0` 无歧义,covers 存
   struct 字段。
2. **门点实现**:`UringConn.held_watermark: Option<u64>`;stamp 在
   三个 dispatch 面(uring_on_recv / bigbulk feed / **B2-alt
   dispatch_bareset_owned——Explore 图漏的第三面**,its +OK 同样必须
   持有),用 w0/w1 差分只 stamp 真追加了记录的批。arm 循环 gate 的
   是 output→write_buf swap;needs_more 见 output 非空自动 re-queue。
3. **fsync 失败策略**:定为 hold+无限重试(水位不动 → 无假 ack,
   客户端可自行超时),不做 retry-once-then-disconnect —— 断连机
   构复杂度不值;比今天同步路径的「log 后照样回复」严格。

额外收紧:**run_swap 在 rename 后 fsync 目录**(数据面各路径本就
sync_all,rename 的目录项是唯一缺口;swap-as-durability-proof 需要
它)。目录 sync 失败不反悔已落地的 rename,响亮日志。

验证状态见 finding(bench/FINDING-2026-08-12-s2-*.md,盒上跑完补)。
