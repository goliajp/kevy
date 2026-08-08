# RFC: V3 AOF 追加下车 — 把 write(2) 挪出 reactor 线程

> Phase A 结论(`bench/FINDING-2026-08-08-tail-stall-is-the-aof-write-syscall.md`):
> tailgate 两个 RED cell 的多秒 reactor 停顿同源 = reactor 线程上的
> AOF `write(2)` 在 GB/s 摄入下被内核脏页节流停车;fsync 档位只改
> 停顿的疏密形状。`--no-aof` 双 bar 全过(gap 49ms)= 上限证明。

## 0. 约束(不许动的)

- **持久语义不降级**:`appendfsync always` 的回复必须仍然等到本批
  追加(含 fsync)真正落盘;`everysec` 的丢失窗口仍 ≤1s。
- crashgate/repligate 的既有契约全保;AOF 格式不变。
- 纯 Rust 零依赖;io_uring 绑定已在 kevy-sys。

## 1. 方案

### A. io_uring SQE 化(推荐方向)
AOF 追加改为 reactor 自己的 io_uring 上的 `write` SQE(链式,同批
`fsync` SQE 跟队):reactor 永不同步陷入 write(2);`always` 的回复
在 CQE 到达后由既有 pending-slot 机制放行(与跨分片回复同构——
**架构上是把"磁盘"当成又一个异步对端**)。
- 优点:零新线程;顺序性由 SQE 链保证;kevy 的 uring 基建全在;
  epoll 构建走 B 案兜底。
- 风险:O_APPEND + uring write 的 offset 语义(-1 offset 支持已查?
  需实证);CQE 慢时 pending 回复队列的内存界(慢盘 = 天然背压,
  需要 bound + 触界策略 = 拍板点②)。

### B. 专职 AOF writer 线程(兜底/epoll 路径)
每 shard 一个 writer 线程 + 有界 SPSC 环:reactor 只 memcpy 进环;
`always` 批的回复挂在环序号上,writer 完成后经既有 inbox 唤醒放行。
- 优点:实现直白;两种 reactor 同一形状;persist_worker 先例在。
- 风险:每 shard 多一线程(8 shard = +8);环满语义 = 同拍板点②。

### C. 只把 fsync 下车(Redis bio 形)
**已被 Phase A 证伪为不充分**:停顿在 write(2) 不在 fsync(2)
(fsync=no 时停顿反而更大)。列出仅为完整性,不推荐。

## 2. 拍板点

| # | 事项 | 建议 |
|---|---|---|
| ① | 方向:A(uring SQE)还是 B(writer 线程),或 A 主 B 兜底 | A 主 B 兜底(uring 是默认 reactor) |
| ② | 慢盘背压语义:环/队列满时 reactor 是阻塞(=今天的行为)还是断连/报错 | 阻塞(与今天等价,但只在真慢盘时;正常态零阻塞) |
| ③ | everysec 丢失窗口的精确口径(off-thread 后 fsync 计时归谁) | writer/CQE 侧持钟,窗口语义不变 |
| ④ | mixed 残余 310ms(巨 list realloc 嫌疑)是否本 train 一并收 | 先 decomp 定性再定(独立小切片) |

## 3. 验收

tailgate 双 cell 双 bar 全绿(p99.9 ≤100ms 且 reactor gap ≤100ms)
+ crashgate/repligate 不变绿 + perfgate-median 无回归(±floor 内)。

## 4. 实施切片(拍板后)

**进度(2026-08-08)**:S1 persist 半场已落(`Aof` queued-append 模式:
`enable_queued_appends`/`take_pending`/`queued_fd` + 结构性入口诚实回退,
`97151955`,逐字节等价钉死)+ uring 原语已落(`prep_write_at`/`prep_fsync`,
`e2a54785`)。**S1 余 = reactor 热循环接线**(在途 chunk 跨 CQE 存活 /
fsync 排在在途写之后 / rotation 前 drain / everysec fsync 调度 / epoll
兜底)—— 崩溃安全爆炸半径大,建议新鲜上下文开工。

切片:
S1 uring `write` SQE 化(everysec/no 先行,回复不等待)→ S2
`always` 的 CQE-gated 回复 → S3 epoll 兜底(B 案短环)→ S4 tailgate
转绿 + 三门禁复跑 → S5 finding 收尾。
