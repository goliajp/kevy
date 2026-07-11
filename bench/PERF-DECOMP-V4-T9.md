# PERF-DECOMP-V4-T9 — 全景 decomposition:kevy 质变杠杆(v4 T9a / K-901)

Date: 2026-07-11 · Phase A read-only research(perf-vs-foss 两步 dance)
Box: lx64(i7-10700K 8C16T @3.80GHz,Linux 6.12.90,mitigations 见 2026-06-20 记录)
kevy: 3.18.0 @ feature/v4 05236c1b(io_uring reactor,8 shards,SO_REUSEPORT)
Workload 主轴: `redis-benchmark -c 50 -P 16 -n 8M -t get,set`(arena.sh fair-fight:
server cores 0-7,client cores 8-15,median-of-5)

**目标**:找 ≥1.5× 单主轴 step-change 的结构性机会,不是 polish 清单。
旧结论"15.4µs/op 里 kernel TCP 占 8-12µs"作为**假设重验**,不作前提。

---

## 0. 结论速览

**四方**:kevy 6.39M GET / 6.38M SET(c50 P16)= 1.60× redis8、3.00×
valkey、3.60× dragonfly —— 全场领先,"对手有结构招可抄"整体证伪
(dragonfly 垫底,valkey 烧锁,redis8 单 exec 线程是墙)。

**kevy 自身的三层真相**(全部实测,median-of-N + perf + 计数):
1. c50 主轴 6.39M 是**欠饱和 ceiling**,不是容量:c100/P64/c200/dual
   四证 +25% 到 **8.0M 平台**;同 binary cycles/op 4,700 → 3,644。
2. **cross-shard 架构税是第一税源**:单 shard 一核 3.55M = 8-shard
   布局的 56%;转发链直接账面 ~580 c/op(12%),spread 形态(-r 1M,
   真实世界形)再 +65% cycles/op(7,735)。
3. 旧"kernel TCP 8-12µs 是墙"结论**只适用 bigval 形态**;-d 3 主轴
   kernel ≈ 0.55µs·core/op(44%),user 是大头。

**Top-5 杠杆**(§6):
| # | 杠杆 | 轴 | 估收益 | 归类 | blast |
|---|---|---|---|---|---|
| L1 ⭐ | shared-read keyspace(GET 去转发,origin 直读) | GET/读轴 | spread ~2.0×,主轴 +25-55% | per-op 结构 | 最大(数周) |
| L2 ⭐ | c50 欠饱和闭合(idle-ladder → 阻塞 enter 重设计) | 主轴 c50 | +18-25%(兑现 8M 平台) | per-iter | 中 |
| L3 | run_uring per-CQE → per-conn 批处理 | plateau | +10-18% | per-op 常数 | 中小 |
| L4 | map hugepage/单线布局(spread 税) | spread | +8-15% | per-op | 小-中 |
| L5 | io_uring 特性篮子(direct-fd/BUNDLE/MSG_RING) | 全 | +3-8% | polish | 小 |

唯一 ≥1.5× 质变量级 = **L1**(spread GET 轴 ~2.0× 想象空间),需先过
读道原型 gate(§6 L1);L2+L3+L5 叠加可把主轴推向 ~9-10M(合计
+40-55%),属强 polish 链而非单点质变。

---

## 1. Phase A-1 四方实测(2026-07-11,median-of-5 + sample stdev)

### 1.1 四方吞吐表(-c50 -P16 -n8M,默认 -d,默认单 key)

| server | 配置 | GET | SET |
|---|---|---:|---:|
| **kevy 3.18.0** | --threads 8, io_uring auto | **6,389,776 ±2.8k** | **6,384,676 ±582k** |
| redis 8 (8.4.x) | --io-threads 8 | 3,996,004 ±198k | 2,460,024 ±112k |
| valkey 9.1 | --io-threads 8 | 2,132,196 ±68k | 1,598,402 ±0.1k |
| dragonfly (latest, 2025-06 image) | --proactor_threads=8 | 1,776,199 ±42k | 1,453,224 ±28k |

- kevy = 1.60× redis8、3.00× valkey、3.60× dragonfly(GET)。
- **意外证伪**:shared-nothing 同门 dragonfly 在此形态**垫底**——它的
  transaction/fiber hop 税(§4)在 c50 pipelined 单 key 形态吃掉全部
  io_uring 高级机制收益。"对手有 kevy 没有的结构招"这个预设在本形态不成立。
- 数字量子化注意:redis-benchmark 的 rate = n/elapsed,elapsed 粒度 ~ms,
  8M/1.25s 档位会出现跨 server 撞数(3,996,004 同时出现在 redis8-GET 与
  历史 kevy-HSET);gap ≫ 量子化误差,判定不受影响。

### 1.2 Workload 形态 probe(kevy,GET 除注明外)

| shape | 吞吐 | vs c50 基线 |
|---|---:|---:|
| c50 P16 6t(基线) | 6,389,776 | — |
| c50 P16 **8t** | 6,389,776 | ±0(client 线程加宽不动 → client 不是瓶颈) |
| **c100** P16 8t | **7,976,072** | **+24.8%** |
| c50 **P64** 6t | **7,994,670** | **+25.1%** |
| c200 P16 8t | 7,920,792 | +24.0%(平台) |
| dual 2×(c25 4t) | 3,994,674 ×2 = 7,989,348 | +25.0% |
| c100 P16 8t SET | 5,326,232 | (SET 高并发反而低于 c50 中值 — SET stdev 大,见 §5) |

**发现 #1(本 decomp 的第一根杠杆证据)**:6.39M 不是 kevy 容量,
是 **c50 形态的欠饱和 ceiling**。conn 密度 50/8 ≈ 6.25 conns/shard 时
busy-poll 反应堆 per-iter 摊销失败(park/spin 往返),c100(12.5 conns/shard)
或 P64(单 iter 更多帧)都直接 +25% 到 **~8.0M 平台**。与 v1.30
`--accept-shards` finding(-d 65536 +10.6%)同族,但本次在**默认 -d 3 主轴**
上量到了 +25%。8.0M 平台才是当前 build 的真容量线。

### 1.3 单 key 形态确认(stage 拆解的前提)

redis-benchmark **不带 `-r` 时 key 模板不随机化**(redis 8.4 源
`redis-benchmark.c:571``if (config.randomkeys) randomizeClientKey(c);`,
`:1443-1446` 仅 `-r` 置位)→ 主轴 workload 是**单 key** `key:__rand_int__`
(16B 字面量)。含义:kevy 侧该 key 唯一 owner shard;~7/8 conns 的请求走
cross-shard RequestBatch 转发。runtime 验证见 §5 probe(DBSIZE)。

---

## 2. Phase A-1 perf 对照(server 侧,-F 499,25s 采样 + 10s perf stat)

### 2.1 cycles/op 记账(perf stat 10s ÷ 实测吞吐)

负载 = get,set 交替 8M/pass 连续循环(与 §1.1 同 shape)。

| server | cycles/10s | 混合吞吐 | cycles/op | 8 核占用¹ |
|---|---:|---:|---:|---:|
| kevy c50 | 300.3G | ~6.39M/s | **~4,700** | ~99%(含 spin) |
| kevy c100 | 291.5G | ~8.0M/s | **~3,644** | ~96% |
| redis 8 | 130.9G | ~3.2M/s | ~4,090 | ~43%(exec 主线程瓶颈) |
| valkey 9.1 | 368.4G | ~1.87M/s | ~19,700 | ~121%(turbo,全烧锁) |
| dragonfly | 167.5G | ~1.6M/s | **~10,400** | ~55%(阻塞等待多) |

¹ 以 8 核 × 3.8GHz 基频 = 304G/10s 计;>100% = turbo。

- **kevy c50 vs c100:同一 binary,conn 密度翻倍 → cycles/op −22%
  (4,700 → 3,644)且吞吐 +25%**。差值 ~1,050 cycles/op 是 c50 形态的
  per-iter 空转税(空 enter、spin、park 往返、低密度 arm loop)。
- valkey 的 cycles/op 是 redis8 的 ~4.8×:self-time 前 8 名全是
  `libc pthread_mutex_lock`(io_thd_1-7 合计 ~33%)+ libc spin,dso 面
  libc 52.7%。valkey 9.1 io-threads(主线程 exec + io 线程忙等交接)
  在 P16 高压下把 8 核烧在锁上——这是它 2.13M 的直接原因。
- redis 8 干净:`call` 2.79% / `prefetchCommands` 2.50%(8.0 lookahead+
  prefetch 生效),kernel 39%,IPC 1.48。4.0M 的瓶颈 = 单 exec 主线程
  (io 线程只做 read/parse/write,§4.2)。

### 2.2 kevy 符号化 self-time(release-perf,c50 / c100,get+set mix)

| symbol | c50 | c100 | c50 cycles/op(×4700) |
|---|---:|---:|---:|
| `Shard::run_uring`(busy-poll 主体,arm loop 等 inline) | 19.28% | 24.54% | ~906 |
| `Shard::dispatch_batch`(parse+prefetch+handle_command) | 6.31% | 5.73% | ~297 |
| `Shard::start_single` | 2.76% | 2.29% | ~130 |
| `drain_inbound_core_slow`(跨 shard 收件) | 2.17% | 1.79% | ~102 |
| `Shard::fold`(seq 环 reply 归并) | 2.00% | 1.94% | ~94 |
| `shard_of`(key hash 路由) | 1.65% | 1.39% | ~78 |
| `push_pending_slot` | 1.34% | 1.24% | ~63 |
| `ArgvView::copy_into`(转发 owned 物化 memcpy) | 1.23% | 1.07% | ~58 |
| `dispatch_with_proto`(owner 侧 verb dispatch) | 1.20% | 1.75% | ~56 |
| `KevyMap::find_by_borrow`(store 查找) | 1.20% | 1.22% | ~56 |
| `start_command`(route 解析) | 1.18% | 1.02% | ~55 |
| `Store::set_value`(SET 半场) | 1.05% | 0.84% | ~49 |
| `drain_front`(reply 环 → conn.output) | 0.86% | 0.85% | ~40 |
| `run_dispatch`(owner 侧封装) | 0.75% | 0.98% | ~35 |
| kevy 其余长尾 | ~5.7% | | ~268 |
| **kernel 合计**(TCP/netfilter/enter/task_work) | 44.15% | 35.27%+nf 5.9% | ~2,075 |
| libc(syscall wrapper、memcpy) | 7.11% | 6.75% | ~334 |

**cross-shard 转发链自加总**(start_single 转发臂 + copy_into +
flush_requests(inline)+ drain_inbound + run_dispatch + dispatch_with_proto +
fold + drain_front + push_pending_slot + shard_of)≈ **12% ≈ 580 cycles/op**
—— 这是把 7/8 请求搬到 owner 再搬回来的直接账面成本(不含它对
batch 密度 / cache 的间接效应,间接部分见 §5 spread 形态)。

**旧结论重验**:kernel ≈ 44%(~0.55 µs·core/op),**"kernel TCP 占
8-12µs/15.4µs" 的 v1.29 结论只适用于 -d 65536 bigval 形态,在 -d 3
P16 主轴上 kernel 不是墙**(user 56% 才是大头)。

---

## 3. kevy 侧 stage 拆解(GET @ c50 P16,io_uring reactor,8 shards)

原子成本表:沿用 PERF-DECOMP-2026-06-28 的 lx64 校准(L1 1ns / DRAM
60-120ns / 小 alloc 35-60ns / KevyMap hit 30-50ns / 跨核 cache line
40-80ns)。budget 锚 = **4,700 cycles/op(c50 实测,§2.1)≈ 1.24 µs·core/op**。
角色:**origin shard**(SO_REUSEPORT 拿到该 conn 的 shard)与 **owner
shard**(`shard_of(key)` 的键主);单 key 主轴上 owner 唯一,~7/8 op 走
转发链(S07-S14),~1/8 op 走 inline 快路(S07′)。

| # | stage | kevy path(file:line) | atomic ops | est c/op(摊 op) | 实测锚 |
|---|---|---|---|---:|---|
| S01 | 客户端帧进入 kernel → TCP rx → multishot recv 填 pbuf slab | kernel(`tcp_rcv_established`→io_uring buf_ring copy) | 每 16KB slab 装 ~数十帧;每帧摊 tcp rx+copy | ~700 | kernel 44% 的 rx 半场 |
| S02 | CQE 收割 + op-tag 分派 | `uring_reactor.rs:177-300`(`for_each_completion`→match) | 每 CQE:1 ring 读 + tag mask;OP_RECV 热臂前置(E11) | ~30 | run_uring self 一部分 |
| S03 | recv 完成处理:pbuf 借出、input 空 → A1 parse-from-slab | `uring_io.rs:89-233`(`uring_on_recv`) | 2-3 map probe(conns/io)+ slab slice | ~60 | 〃 |
| S04 | RESP multibulk 解析(2-arg,借用 argv 零拷贝) | `kevy-resp/request_borrowed.rs:68-103` | 每 arg 1 次 find_crlf+parse_bulk_len;无 alloc(range 记录) | ~50 | dispatch_batch 297 的一部分 |
| S05 | key prefetch + verb resolve(单次 match) | `inbox.rs:37-40` + `KevyCommands::resolve` | 1 prefetch hint + 1 verb match | ~40 | 〃 |
| S06 | conn probe + seq 派发 | `exec.rs:30-47`(`handle_command`) | 1 KevyMap probe + seq++ | ~35 | 〃 |
| S07 | route → `shard_of` hash(hashtag 扫描 + FxFmix) | `exec.rs:200-258` + `reduce.rs:408-427` | 16B hash + tag scan | ~78 | shard_of 1.65% |
| S07′ | (1/8)inline 快路:GET 特判 → `get_into_output` 直写 conn.output | `exec_dispatch.rs:125-216` + `kevy-store/string.rs:56-90` | 1 map probe + header 编码 + ≤30B memcpy | ~90(该 1/8 op 全程) | find_by_borrow 一部分 |
| S08 | (7/8)转发物化:pool Argv `copy_into`(cmd+key ~25B memcpy)+ request_batch push | `exec_dispatch.rs:104-108` | 1 pool take + 25B copy + Vec push | ~75 | copy_into 1.23% + start_single |
| S09 | `flush_requests`:per-iter per-target 批量 ring push + dirty `fetch_or` | `exec.rs:342-358` + `shard_flush.rs:78-100` | 每 batch 1 ring push + 1 Release fetch_or,摊到 ~batch 内 op | ~25 | run_uring inline |
| S10 | owner 收件:dirty swap + ring pop + batch 迭代 | `inbox.rs:196-270`(`drain_inbound_core_slow`) | 1 AcqRel swap + 每 batch 1 pop + 跨核 cache line 拉取 | ~102 | 2.17% |
| S11 | owner 执行:verb dispatch → `KevyMap::find_by_borrow` → reply_scratch → `SmallReply` inline copy(≤30B 栈上) | `exec_op.rs:21-42` + kevy-map | 1 map probe(单 key 恒 L1 热)+ 23B copy,无 alloc | ~150 | dispatch_with_proto+run_dispatch+find |
| S12 | owner 回批:resps.push + `ResponseBatch` send_to(摊批) | `inbox.rs:253-271` | 每 batch 1 ring push + fetch_or | ~25 | 〃 |
| S13 | origin fold:seq 槽定位 + reply 归并 | `exec_fold.rs:18-`(`fold`)+ `push_pending_slot` | 1 conns probe + 槽写 + pending 簿记 | ~157 | fold 2.0% + push_pending_slot 1.34% |
| S14 | `drain_front`:按 seq 序把 reply memcpy 进 conn.output | `reduce.rs`(drain_front) | ≤30B memcpy + 环推进 | ~40 | 0.86% |
| S15 | arm loop:dirty→arm_pending 去重、写侧 `prep_write`/`prep_writev` SQE 构建 | `uring_arm.rs:48-373` | 每 conn/iter 1-2 probe + SQE 写(含 prefetch 流水) | ~250 | run_uring self 大块 |
| S16 | `submit_and_wait(0)`:tail 发布 + `io_uring_enter`(E14 空跳门) | `kevy-uring/ring.rs:361-439` | 每 iter 1 syscall(摊 iter 内 op 数) | ~150 | enter+syscall glue ~5% |
| S17 | kernel tx:writev → tcp_sendmsg → loopback → tcp_ack + nft_do_chain | kernel | 每 conn/iter 1 writev(P16 批 16 reply 一次) | ~1,200 | kernel 44% 的 tx 半场 + nft ~4% |
| S18 | 写完成:`uring_on_write` 进度推进 | `uring_io.rs:341-447` | 1-2 map probe + 状态清 | ~40 | run_uring inline |
| S19 | per-iter 空转税(c50 特有):空 enter、`spin_loop`、park/wake 往返、tick 门 | `uring_reactor.rs:407-481`(idle ladder) | — | **~1,050**(= c50 − c100 实测差) | §2.1 cycles/op 差 |
| S20 | per-iter 杂项:`refresh_clock`、flush_* 位图空检、reap 1/16 门 | `uring_reactor.rs:183-335` | 各 1 分支/iter | ~50 | run_uring inline |

**±20% 对账**:S01-S20 估算合计 ≈ 700+30+60+50+40+35+78+(90×⅛≈11)+
(75+25+102+150+25+157+40)×⅞≈434+250+150+1200+40+1050+50 ≈ **4,180 c/op**,
vs 实测 4,700 c/op → **偏差 −11%,在 ±20% 门内**(缺口主要在 kevy 长尾
符号 ~268 c/op 与 kernel 内 task_work/软中断的归属粗糙)。c100 形态:
去掉 S19 后估算 ≈ 3,130 vs 实测 3,644(−14%,同样门内)。

**结构读法**:
- kernel(S01+S17)≈ 1,900-2,075 c/op ≈ 44%——每 op 平摊的 TCP 收发,
  P16 已把 syscall 数摊薄(S16 只 ~150);再压 kernel 只剩 nft 卸载
  (~190 c/op,盒配置层)与更深批("多 conn 合帧"协议不允许)。
- **user 侧最大单块 = run_uring self 906 c/op(S02/S15/S18/S19/S20 的
  inline 聚合)**,其次 = 转发链 ~580 c/op(S08-S14 的 ⅞ 摊)+
  dispatch_batch 297。
- 单 key 是转发链的**最优情况**(batch 密度 16/iter、owner map 恒热);
  spread(-r 1M)时 batch 密度 /7、map 走 DRAM,吞吐 −29%(§5)。

---

## 4. 对手侧结构模型(源码已读,file:line;详证据链见 agent 报告归档)

### 4.1 dragonfly(shared-nothing 同门,helio proactor)

源:helio @ c054a1ac(dragonfly pinned submodule)+ dragonfly HEAD。

- ring:1024 entries(`dfly_main.cc:1201`),**SUBMIT_ALL + DEFER_TASKRUN +
  COOP_TASKRUN + TASKRUN_FLAG + SINGLE_ISSUER**(`uring_proactor.cc:184,199-200`),
  **无 SQPOLL**(明确注释弃用 `:212`),ring fd 注册 + direct-fd 表。
- 提交:per-iter 批量 `io_uring_submit_and_get_events`,`SQ_TASKRUN` 位
  门空跳(`:831-839`)—— 与 kevy E14 同目的不同机制。
- recv:**multishot + buf_ring + IORING_RECVSEND_BUNDLE**(`uring_socket.cc:519-530`),
  buf 1500B × `--uring_recv_buffer_cnt`;完成后 memcpy 进 conn 的
  `io_buf_`(`dragonfly_connection.cc:3249`)—— 与 kevy 的 16KB slab +
  parse-from-slab 相比**多一次全量 copy**。
- send:PrepSend/PrepSendMsg(**无 SEND_ZC**);reply 在
  `SinkReplyBuilder` 聚 iovec,V2 io-loop **park 前才 flush 一次**
  (`dragonfly_connection.cc:2987-2996,3526`)= N pipelined replies 1 次
  sendmsg —— 与 kevy per-iter writev 等价。
- 跨线程:fiber 挂起/恢复 boost.context(同线程 0 atomic;跨线程 2
  atomic + MPSC push + **MSG_RING** SQE 唤醒,`uring_proactor.cc:1059-1078`)。
- 单 key GET 流:conn 线程 ≠ owner 线程时走 transaction
  `ScheduleSingleHop` → shard FiberQueue → 执行 → run_barrier 回唤,
  **≈2 次跨线程唤醒 + 2-3 次 fiber 切换/op**(`transaction.cc:731-1000`);
  同线程时 `CanRunInlined`(`transaction.cc:1691-1710`)0 hop。
  pipeline 下 `MultiCommandSquasher`(`multi_command_squasher.cc:114-324`)
  把 N 条同 shard 命令并成 1 hop。
- **实测裁决(§1.1):这一整套在 c50 P16 单 key 上只有 1.78M/s,
  垫底**。机制在,税更重:transaction/fiber/squash 的调度层比 kevy 的
  裸 RequestBatch 贵一个量级。**"dragonfly 有 kevy 缺的结构招"被证伪;
  反向结论成立 —— kevy 的 message-batch 转发已是同门最省形态。**
  可借鉴残值:RECVSEND_BUNDLE(recv 侧合并 CQE)、MSG_RING(替 waker
  pipe)、direct-fd 表(省 per-op fget/fput)—— 全部 polish 量级。

### 4.2 redis 8(io 线程新架构)

源:redis 8.4.4。io 线程各自跑**独立 ae event loop**,read+parse+write
全在 io 线程(`iothread.c:712-743,614-618`);exec 仍单主线程
(`networking.c:3143-3150` CLIENT_IO_PENDING_COMMAND 交接,mutex+eventfd
+ running/paused atomic 消唤醒,`iothread.c:25-67,328-398`)。
lookahead 解析 + `prefetchCommands` 批预取(`iothread.c:347-369`,
`networking.c:3020-3127`)。**无 io_uring**。
- 实测:4.0M GET,cycles/op ~4,090,8 核只用 ~43% —— **exec 主线程是
  它的墙**;kevy 赢它的本质是 exec 并行(8 shard)。它赢 valkey 的
  本质是交接消锁(valkey 33% 时间在 pthread_mutex_lock)。

### 4.3 valkey 9.1(io-threads 8)

perf:libc 52.7%(`pthread_mutex_lock` 33% + spin),kernel 仅 6.6%,
19,700 cycles/op —— io 线程互斥交接在 P16 高压下全烧在锁上。
(其 io-threads 模型的 file:line 拆解见 2026-06-28 c100-GET decomp,
本轮不重复。)

---

## 5. Runtime 验证 probe(方法论 gate:decomp 声称必须配实测计数)

### 5.1 workload 形态验证(probe2,2026-07-11)

| probe | 结果 | 含义 |
|---|---|---|
| 默认 GET 后 `DBSIZE` | **0** | **默认主轴 GET 全是 miss**(`$-1`,连 key 都没建);arena get,set 混跑时首 pass 后有 1 key,后续 GET 是 3B 值 hit。对四方公平但要入账 |
| `-r 1M` 后 `DBSIZE` | 999,634 | spread 形态生效 |
| GET -r1M c50 | 4,566,210(−29% vs 单 key) | spread = batch 密度 /7 + map DRAM miss;**单 key 是转发链最优情况** |
| GET -r1M c100 | 5,319,149 | conn 密度红利在 spread 下仍 +16% |
| SET -r1M c50 | 4,563,605 | |
| accept-shards 2/3/4(-d 3 单 key) | 4.00M / 5.33M / 6.39M | **accept-shards 在小值主轴单调有害**(v1.30 的 +10.6% 仅 bigval 成立);-r1M 下更惨(2.28/2.66/2.91M)。parse/forward 是计算主体,砍接入 shard = 砍并行 |
| --threads 1(单 shard,零转发)GET c50 | **3,552,398** | 单 shard 一核 = 8 shard 布局的 56%!8× 核只换 1.8× |
| --threads 2 / 4 / 8 GET c50 | 4.00M / 6.39M / 6.39M | 4 shard 即打平 8 shard(c50 形态);shard 扩展在此形态早饱和 |

### 5.2 probe3(dragonfly perf 重测 + kevy spread perf + strace,2026-07-11)

**kevy spread(-r 1M,c50 GET,release-perf)**:

| 指标 | 单 key(§2) | spread -r1M | Δ |
|---|---:|---:|---|
| cycles/op | 4,700 | **7,735**(353.4G/10s ÷ 4.57M/s) | +65% |
| IPC | 1.12 | 0.87 | DRAM bound |
| `find_by_borrow` | 1.20% | **8.29%** | map probe 走 DRAM |
| `drain_inbound_core_slow` | 2.17% | 4.45% | 收件批变稀 |
| `send_to` | (inline 不可见) | 1.85% | 7 target 稀疏 push |
| `dispatch_batch` | 6.31% | 4.22% | |
| malloc/cfree | — | ~2.1% | spread 下 pool 命中掉 |
| kernel dso | 44.15% | 44.02% | 份额同,绝对值 +65% |

**kevy syscall 形态(strace -c -f 10s,c50 GET)**:io_uring_enter
2.40M calls/10s(77.5% time)≈ **每 enter 摊 ~27 ops**(6.39M×10 ÷ 2.4M;
strace 减速,比值仍指示性)→ P16 下 syscall 已深度摊薄,S16 估算成立;
其余 syscall 合计 ~4%(0.1M/10s)。

**dragonfly perf(对准真 server 子进程,25 threads;容器 init 是壳,
首轮采到壳 = 空,已修)**:cycles/op ≈ **10,400**(167.5G/10s ÷ ~1.6M/s),
dso:user 55.9% / kernel 37.3% / libc 4.1%;8 核占用仅 ~55%(hop 等待)。
= kevy c50 的 2.2×、c100 的 2.9× per-op 成本 —— §4.1 的
transaction/fiber hop 税的直接量化。静态二进制符号未解析(容器内
无 debuginfo),user 侧细分从源码模型(§4.1)推,不再深钻(它已不是
标杆)。

## 6. 杠杆候选表(产出主体;归类 per-op / per-iter 依 perf-vs-foss §8 v1.30 分界)

### L1 ⭐ shared-read keyspace:GET 去转发(origin 直读 owner 的 map)

- **机会**:读命令不再走 S08-S14 转发链,origin shard 直接对全局可见的
  keyspace 做 lock-free 读(seqlock/epoch 读道;写仍单写者归 owner)。
  每个 shard 的 GET 都变成 §3 S07′ inline 快路形状。
- **证据**:转发链直接账面 ~580 c/op(§2.2);spread 形态 7,735 c/op
  中转发密度崩塌 + map DRAM 链是单 key 之外的主增量(§5.2 kevy-r1m:
  find 8.29%、send_to 1.85%、drain_inbound 4.45%);t1 单 shard 3.55M =
  8-shard 布局 56%(§5.1)——cross-shard 是当前架构第一税源。
- **估算**:spread GET c100 5.32M → ~10M(消转发 + 全 shard inline +
  xshard spin 消失),**~2.0×**;主轴单 key c50 6.39M → 8-10M。
  写轴不动(写仍转发)。
- **blast radius**:最大。kevy-map 读并发化(per-entry seqlock 或
  epoch-GC)、kevy-store 读时过期/LRU 语义放宽(过期判读本地、回收归
  owner reaper)、kevy-rt 路由分叉(读/写分道)、**同 conn
  写后读一致性 fence**(conn 有 in-flight 转发写时,该 key 读必须
  排队/转发,否则违反 Redis 单线程可见性语义)。数周级;是"质变"
  级候选里唯一够 ≥1.5× 想象空间的。
- **归类**:per-op(结构)。**Phase B 前置 gate**:先做 read-only
  原型量 seqlock 读道的跨核 cache line 代价(单 key hot entry 8 读者
  的 line 弹跳是反向风险)。

### L2 ⭐ c50 欠饱和闭合:idle-ladder → 阻塞 enter 重设计

- **机会**:c50 形态每 op 多烧 ~1,050 cycles(§2.1 c50 vs c100),
  来源 = 低 conn 密度下 spin→nap→park 阶梯的空转与 park/wake 往返
  (`uring_reactor.rs:407-481`)。候选改法:用
  `submit_and_wait(1)` + `IORING_OP_TIMEOUT` 短阻塞替代 spin/park
  状态机(kernel 在 CQE 到达即回,无 waker pipe 往返),或
  自适应 spin 预算(按最近 iter 产出率调 URING_SPIN_LIMIT)。
  跨 shard ring 消息需并入唤醒面(MSG_RING 或维持 waker fd)。
- **证据**:c100 / P64 / dual / c200 全部 +25% 到 8.0M 平台(§1.2);
  同 binary cycles/op −22%(§2.1);accept-shards 集中(v1.30 思路)
  在 -d 3 被证伪(§5.1)——必须在**不减并行度**的前提下修摊销。
- **估算**:c50 主轴 GET 6.39M → ~7.5-8.0M(**+18-25%**,兑现平台)。
- **blast radius**:中。uring_reactor idle ladder + uring_park +
  kevy-uring enter 面;-c1 延迟回归风险(历史 15× 教训,
  `uring_reactor.rs:420-437`)必须过 -c1 gate。
- **归类**:per-iter。

### L3 — run_uring 主体压缩:per-CQE → per-conn 批处理

- **机会**:plateau(c100)时 run_uring self 24.5% ≈ 894 c/op,是
  user 侧最大单块(§2.2)。当前每 CQE 独立走 tag match + 2-3 次 map
  probe + mark_arm_pending;P16 下同 conn 的多个事件可先按 conn 聚
  (一批 CQE 一次 conns/io probe),arm loop 与 completion loop 合并
  一遍扫描。
- **估算**:plateau 8.0M → ~8.8-9.5M(**+10-18%**);与 L2 正交可叠。
- **blast radius**:中小。uring_reactor/uring_arm/uring_io 三文件;
  逻辑等价重排,无语义面。
- **归类**:per-op(每 op 常数压缩)。

### L4 — map/store 内存布局(spread 真实形态税)

- **机会**:-r1M 时 `find_by_borrow` 1.2%→8.29%,IPC 1.12→0.87,
  cycles/op +65%(§5.2)——map probe 走 DRAM。候选:KevyMap slab 用
  2MB hugepage mmap(kevy-sys 已有 mmap 面)、entry 头与 key 内联
  压缩到单 cache line、probe 序预取第二 line。
- **估算**:spread GET +8-15%;单 key 轴 0。
- **blast radius**:小-中(kevy-map/kevy-alloc,石头层,fuzz+bench 完备)。
- **归类**:per-op。

### L5 — io_uring 特性篮子(dragonfly 借鉴残值,polish 级)

- direct-fd 表(`IORING_REGISTER_FILES`,省 per-op fget/fput;
  dragonfly `uring_proactor.cc:258`)≈ kernel enter 内小几 %;
- `IORING_RECVSEND_BUNDLE`(recv 合并 CQE,kernel 6.10+;dragonfly
  `uring_socket.cc:528`)—— kevy 每 conn 每 iter 已单 CQE 多帧,余量小;
- MSG_RING 替 waker pipe(dragonfly `uring_proactor.cc:1073`)——
  仅 park 唤醒路径,c50 有一点,plateau 0;
- `SUBMIT_ALL` flag;nft 卸载(盒配置,非代码,~4-6% kernel)。
- **估算合计**:+3-8%,全 polish 级,不单独立项,可搭车 L2/L3 实施。

### 证伪归档(本轮 decomposition 排除的方向)

| 候选 | 裁决 | 证据 |
|---|---|---|
| SQPOLL | 已证伪(历史 D5:2-15× 回归;dragonfly 也弃用) | `uring_reactor.rs:35-40`,`uring_proactor.cc:212` |
| SEND_ZC | 不适用(≤30B 回复,ZC 注册开销 > 收益;dragonfly 也未用) | §4.1 |
| DEFER_TASKRUN | kevy 实测回归 65-73%(busy-poll 形态不合);dragonfly 用它但配 submit_and_get_events 形态 | `kevy-uring/ring.rs:205-209` |
| accept-shards 集中 | -d 3 主轴单调有害(−37%~−50%) | §5.1 probe |
| dragonfly transaction/squash 模型 | 同 workload 垫底(1.78M),调度税 > 收益 | §1.1 + §4.1 |
| "kernel TCP 是墙" | kernel 44% ≈ 0.55µs·core/op,user 56% 更大;P16 syscall 已摊薄(~27 op/enter,§5.2 strace) | §2.2 |

---

## 7. R3 翻盘账(预测 vs 实测)

| 起手预测 | 实测裁决 |
|---|---|
| "kernel TCP 8-12µs 主导 15.4µs/op"(v1.29 旧结论作为假设) | **REFUTED**:-d 3 P16 主轴 kernel ~0.55µs·core/op(44%),user 是大头 |
| "dragonfly 是 shared-nothing 同门标杆,应有可抄结构招" | **REFUTED**:同 shape 垫底(kevy 的 28%);它的 io_uring 高级特性被 fiber/transaction 税吃光 |
| "accept-shards 是 conn-density 杠杆"(v1.30 finding 外推) | **REFUTED**:-d 3 上 accept2/3/4 全部大幅负收益 |
| (未预测)c50 主轴欠饱和 | **发现**:+25% 平台(c100/P64/c200/dual 四证);cycles/op −22% |
| (未预测)默认主轴 GET 全 miss、单 key | **发现**:DBSIZE=0;-r 1M spread −29%,单 key 是最优形态 |
| (未预测)单 shard 3.55M = 8-shard 的 56% | **发现**:cross-shard 架构税是第一税源(→ L1) |

---

## 8. 收尾纪律记录

- lx64 清盒:已完成(arena4 容器 rm -f、kevy/redis-benchmark 进程
  pkill、/tmp/arena4-work + perf/pstat/strace 数据 + probe 脚本全清,
  残留复核 = 0;本轮外的历史脚本未动)。
- 本 doc git add 不 commit(Phase A read-only 交付)。
- Phase B 双 gate(方法论 §9):L1 动工前必须 (a) 重验 gap 非 noise
  (已做:median-of-5 + 四证平台),(b) perf-record 验证攻击面
  ≥ 双位数 pp(已做:转发链 12% 直接 + spread 间接;L1 原型 gate 见
  §6 L1 条目)。
