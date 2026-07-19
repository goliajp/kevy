# FINDING(已闭):io_uring 多 shard + pub/sub 扇出下的 recv 重臂丢失 = 连接永久 wedge(2026-07-18)

## 根因(已定位并修复)

`uring_arm.rs` 的 recv 重臂:`prep_recv_multishot` 因 **SQ 环瞬时满**(pub/sub 扇出的一批写 SQE + 多 conn 同轮 arm)返回 false 时,`recv_armed` 留 false,但当轮的 `needs_more` 只判 write/output 有无残留、**不含"想重臂却没臂上"** → 该 conn 被踢出 `arm_pending` 队列,再无任何事件把它拉回(客户端正阻塞等一条它自己那条请求的回复,不会再发字节触发,也没有待写 output)。结果:连接永久卡死,征状正是 `CLIENT LIST` 里 `cmd=NULL events=r`,reactor 因其余 conn 空转在 `io_uring_enter`。

**修复**:记录 `recv_arm_deferred = 想重臂 && 这轮没臂上`,并入 `needs_more`,让该 conn 留在队列下轮重试(submit 排空 SQ 后即成功)。

**验证**(lx64,内核 6.12.95,16 核):补丁前完整 node-redis 模式 12 次跑 **9 挂**;补丁后同循环 15 次 **0 挂**。rt/uring 单测全绿,本地 clientgate 六客户端 PASS。CI 的 `KEVY_IO_URING=0` 诊断盾已撤,clientgate 恢复默认 uring 以守住此修复。

## 关键更正:不是 "x86" 特异,是**核数相关**

初判写成 x86-io_uring 特异是错的。真触发面 = **io_uring reactor + 多 shard(核多→默认 shard 多)+ 客户端全模式(多连接含 pub/sub 副连接,足以造出扇出写把 SQ 压满的那一瞬)**。arm64 容器"过"只是因为核少→默认单 shard→不进多 shard 扇出路径;GH x86 runner 与 lx64 核多→多 shard→命中。epoll 恒过是因为 epoll 后端不经这条 uring SQ-满 重臂门。是竞态(负载相关),非确定性——冷跑偶过、热机后稳挂。

---

## (原始记录)时间线与判别过程

以下为定位过程留痕。

## 症状

自 2026-07-18 ~01:00 UTC 起(此前同代码全绿),GH `ubuntu-latest`(x86_64)上所有**跑 kevy server 且未禁 uring**的门开始悬挂:

- **clientgate / node-redis 腿**:逐命令仪表定位 —— `connect` 成功、`PING`→`PONG` 成功、`FLUSHALL` 成功回复,**其后第一条 `SET` 永不回**;180s 超时时该连接已从 `CLIENT LIST` 消失(server 侧只剩 kevy-cli 自己)。server 日志无任何错误行,明确打印 `reactor = io_uring (io_uring available)`。
- **conformance go / ts-node / ts-bun 的 blocking-pop 腿**:`TestBlockingPops/remote` 等在 blpop 处 10-20s 超时(多轮 rerun 时好时坏,后期趋于常挂)。
- **availgate**(contract-gates job):phase 4 clamp 13 的 setup 段(kill 旧进程 → `wait_ports_free`)零输出悬挂直到 job 25 分钟超时——形状与"server 进程 SIGTERM 后不退出/端口不释放"一致。

## 判别矩阵(全部实测)

| 环境 | reactor | 结果 |
|---|---|---|
| GH runner x86,uring(默认) | io_uring | **挂**(5+ 连续,确定性) |
| GH runner x86,`KEVY_IO_URING=0`(判别 commit `004515a6`) | epoll | **clientgate 六客户端全绿** |
| macOS 本机(aarch64) | kqueue | 全绿(clientgate/availgate/blpop) |
| Docker linux/arm64 容器(node:24 client × 容器内 linux kevy,AOF on) | io_uring 可用 | 全绿 |
| GH runner x86,单元/集成测试 | 测试全程 `KEVY_IO_URING=0` | 全绿(所以测试矩阵从来测不到这条路径) |

排除项:GH 镜像版本论(同版本 20260714.240.1 既有过也有挂)、npm 上游发版论(redis@6.1.0 是 7 月 1 日)、GitHub 状态页(全绿)、代码因果(时间分界前后的 commit 只动了 embedded 测试面与测试文件,server 二进制同源)。

## 结论与嫌疑

悬挂钉死在 **x86 + io_uring 网络路径**;时间分界 + 代码不变 ⟹ 最强嫌疑 = **runner 车队的宿主内核滚动升级**改变了 kevy 依赖的某个 io_uring 语义。按征状(回复通道突然死亡、阻塞唤醒丢失、退出不释放),第一嫌疑面是 **multishot recv 的 re-arm/cancel 状态机**(v1.29 B2-alt 引入)与新内核的交互;AOF 文件写不走 uring,已排除。

## 状态与下一步

- CI 的 clientgate 暂以 `KEVY_IO_URING=0` 运行(ci.yml 内注明是**诊断态**,不是长期姿势——它掩盖的是真产品路径)。
- 深挖需要可控内核的 x86 真机:**lx64 恢复上线后**,先 `uname -r` 对照 runner 内核,能复现则按 perf-vs-foss 纪律走 decomposition(strace/uring trace 逐 SQE 对账)。
- conformance blpop 腿与 availgate 的悬挂尚未逐一验证同因,但形状一致;uring 修复后应一并复验。
- 若 lx64 内核较旧无法复现,备选:Docker x86 模拟(qemu,慢)或临时 x86 云盒。

## 时间线证据锚

- 00:42 UTC clientgate 同代码 2 分钟绿(run 29623659811)→ 01:38 起同 job 三连挂(run 29625584540 的三次 attempt)→ 06:59 步进仪表捕获 SET 悬挂(run 29634868553)→ 07:4x epoll 判别绿(run 29635671813)。

---

## 续:第二个 uring recv bug —— multishot res=0 + F_SOCK_NONEMPTY 被误判 EOF(2026-07-18 深夜,已修 `667005f9`)

首个 wedge(`76c79c38`,SQ 满 recv 重臂丢失)修好后,CI 的 **client-conformance blpop/bzpopmin 腿仍 flaky 挂**(ts-node/ts-bun,20s 超时,多夜"累犯")。这是**第二个独立的 uring recv bug**。

### 本地 uring 复现平台(可复用)

lx64 掉线期间发现:**Docker + OrbStack VM(内核 7.0)+ `--security-opt seccomp=unconfined`** 就能在本机容器拿到 io_uring(默认 seccomp 挡 `io_uring_setup`)。忠实复现 CI conformance:
- 容器内构建 linux `kevy` + `libkevy_napi.so`(`rust:1` 容器,cargo cache 挂载)
- `node:24` 容器跑 `bindings/ts` 的 `npm run test:node`,`KEVY_NAPI_LIB` 指 linux napi、harness spawn linux uring kevy
- 命中率 ~1/10。**注意 OrbStack 7.0 是自定义内核,与真目标(GH runner Ubuntu 6.x / lx64 6.12)行为可能不同**。

### 根因(全 trace 定位)

逐步 probe(每 backend/每命令打印)锁定:挂的**不是 blpop,是 remote uring server 的 `BZPOPMIN` 立即命中**,且在 `BLPOP empty 100ms 超时`之后。server 侧 `CLIENT LIST` = `cmd=NULL events=r`(BZPOPMIN 请求没被 dispatch);`ss` = 双端 **ESTABLISHED、Recv-Q=0、172 字节全收全 ack**(不是 EOF,字节被 io_uring 收进 provided buffer)。

服务端全 io_uring trace(每完成打 res/flags/has_more/buffer_id)给出铁证:卡死 conn 在 ZADD(res=52)后**直接收到 `res=0 flags=0x4 has_more=false bid=None`** —— `0x4 = IORING_CQE_F_SOCK_NONEMPTY`:F_MORE 已清(multishot 终止)但 **socket 仍有数据**(那 33 字节 BZPOPMIN)。内核在说"重臂我排空剩余",**不是** EOF。成功的 conn 则先收 `res=33` dispatch BZPOPMIN 再收 res=0(客户端真关闭的 EOF)。

kevy 把 `res <= 0` 一律当 EOF/错误 → `mark_closing` → BZPOPMIN 字节烂在 provided buffer 里,活连接 wedge(客户端永等一条服务器消费了却没 dispatch 的请求的回复)。

### 修复

`res == 0` 仅在 **F_SOCK_NONEMPTY 清零**时才是 EOF;置位(或 `-ENOBUFS`)则重臂排空。`Completion::sock_nonempty()` 新增。`res > 0` 时重置 `recv_zero_streak`。**有界守卫**:`recv_zero_streak` 上限 256 —— 若某内核在有数据时反复回 0 长度完成而不排空(OrbStack 7.0 观察到的 re-arm 怪癖),超限即关连接(客户端重连)而非活锁烧 CPU。

### 验证与残留

- rt/uring 单测全绿,严格构建净,locgate 过(helper 移 uring_arm.rs 保双文件 ≤500)。
- OrbStack 容器:修前 idle-wedge(cmd=NULL 永久卡);修后大多过,**残留 ~1/20** 是 OrbStack 7.0 的第二形态(res=0 但 F_SOCK_NONEMPTY 清零的伪 EOF,完成层与真 EOF 不可分,或 re-arm 活锁被守卫截断)。**这是 OrbStack 自定义内核怪癖,真 6.x 内核未必有**。
- **真内核验证 oracle = CI 的 clientgate(已默认 uring 绿)+ client-conformance(真 Ubuntu)**;lx64(6.12 真内核)回线后再本地复验。若 CI conformance 仍 flaky → 说明真内核也有第二形态,需再攻(可能要 F_SOCK_NONEMPTY 清零时也 bounded-retry,或阻塞命令后强制单发 recv 排空)。

### 真内核 A/B 判决(2026-07-18,lx64 恢复后 — 决定性)

lx64(**内核 6.12.95 真主线**,16 核,空载)上跑同一套 `bindings/ts` conformance × 25:

| 二进制 | 结果 |
|---|---|
| 修复前 `76c79c38`(仅含第一个 SQ-满 wedge 修复) | **4/25 挂**(全部 blpop/bzpopmin 20s 超时) |
| 修复后 `a9450c9f`(含 F_SOCK_NONEMPTY 判别) | **0/25** |

⟹ **F_SOCK_NONEMPTY 修复在真主线内核上被证明有效**(4/25 → 0/25),不是主观改善。

两处此前的观测由此归位:
- **OrbStack 的 ~1/20 残留**:确认是其 **7.0 自定义内核独有怪癖**(真 6.12 上 0/25),非产品缺陷。判据以真内核为准这条纪律成立。
- **CI 那次 ts-node 残留**(run 29643489869 首次尝试,rerun 即过):GH 共享 runner 高负载下的更极端时序;lx64 空载复现不到,需要负载才能撞上。若要进一步压这个尾巴,应在**加载状态**下复现,而非空载盒。

### 教训(补 §1 系统性盲点)

- **CI 单元/集成测试全程 `KEVY_IO_URING=0`,产品的 uring 数据面从来没进测试矩阵** —— 这两个 recv bug 都只有 clientgate/conformance 这类"真 server+真客户端"门能抓。**应补一条 uring-on 的多 shard + pub/sub + 阻塞命令压测门**防回归。
- **本地 uring 复现配方(OrbStack + unconfined seccomp)是这次的关键杠杆** —— 远程盒不稳时,本机容器就能开 uring;但要记住其内核非目标内核,判据以真内核 CI 为准。

---

## 第三幕:`uringgate` 与仍未闭合的 wedge(2026-07-18 深夜)

真内核 A/B(4/25 → 0/25)之后,补了本该早就存在的门:`bench/uringgate.sh`。它只做一件事 —— 把两个 bug 各自的触发形状**放进同一个紧循环**:pub/sub 扇出(8 订阅者,填 SQ,同轮多连接抢 arm)+ 阻塞命令超时后紧接更多命令。每个请求带 deadline,收不到回复 = wedge = FAIL。

**结果:conformance 要 25 轮才露 4 次的东西,这门在 lx64 上 ~3 轮内必现。** 门本身有效,但它证明:**两个已发布修复都没把 wedge 关死** —— 更重的负载下仍然 wedge。

### 服务端 trace 说了什么(以及推翻了什么)

对卡死连接(cid=27,round 3)的完整完成序列:

```
res=38 → TR-disp RPUSH bl3     ✓ dispatch
res=31 → TR-disp BLPOP bl3     ✓ dispatch
res=33 → TR-disp BLPOP nk3     ✓ dispatch(超时返回 nil)
res=53 → TR-disp ZADD bz3      ✓ dispatch  ← 命令收到了、也执行了
res=0 flags=0x4 more=false bid=None        ← 伪终止完成落在活连接上
```

⟹ **wedge 不在 recv 侧**:请求被收到并 dispatch 了,**丢的是回复**。而同一个完成形状(`res=0 + F_SOCK_NONEMPTY + bid=None`)在**刚被客户端关闭的订阅者连接**上同样出现 —— 两种情形完成事件逐位相同,正确处理却相反(一个要重臂、一个要关闭)。

### 三个已被实测推翻的消歧方案

1. **只看 F_SOCK_NONEMPTY**(= 已发布的 `667005f9`):活连接不再被误关,但死连接无限重臂空转,**饿死同 shard 的其它连接** —— wedge 换个受害者复现。
2. **有界重试(streak cap)**:cap=256 让死连接空转到把别人饿死;cap=1 又会在伪零上关掉**回复尚未写出**的活连接 —— 回复丢失。两端都错。
3. **`recv(MSG_PEEK|MSG_DONTWAIT)` 权威探针**:思路是让 syscall 定分而非猜。实测仍 wedge;且一次相关性推断(probe 报 EOF vs `ss` 显示 ESTABLISHED)**不成立** —— trace 里没有 cid↔客户端端口映射,tail 到的 probe 行未必属于卡死那条连接。**该假设未被证实也未被证伪,是下一步的第一顺位。**

### 下一步(需要的是证据,不是又一个补丁)

- 给 trace 加 **cid ↔ 客户端端口/fd 的映射**,这样 probe 判定能和外部 `ss` 逐条对质 —— 上面第 3 条悬案就能一次定性。
- 顺着"回复丢失"这条线查**写侧**:ZADD 已 dispatch,回复进了 `conn.output`,之后 arm 访问是否真的提交了 write SQE、write 完成是否回来。recv 侧已被 trace 排除,写侧尚未被同等强度地追。
- 复现门已经在手(`uringgate`,秒级),不必再靠 conformance 的概率。

### 纪律

门以**红色提交**,且**不接入 CI** —— 与 crashgate 当初 `REDpending` 同一套 gate-first 纪律:长期红的必挂门只会训练大家忽略门。已发布的两个修复各自证据独立成立(conformance 4/25 → 0/25),不因这道更重的门未绿而回退。


---

## 第四幕:根因既不在 io_uring,也不在 recv(2026-07-19)

第三幕留下的两条线索,一条走对了,一条是死胡同。

**先说走错的那条。** "顺着写侧查" 确实查出了一个真 bug:chunked-writev 的短写恢复
把 `write_buf` 从下标 0 开始重新线性化,包含了前几个 chunk **已经上线的字节**,而
`write_off` 只记这一个 chunk 被接收的量。两点之间的字节被发送两次。重复前缀对
对端不是"多几个字节",而是 RESP 分帧错位,此后每个回复都解析失败。这个 bug 是
真的,已修(`17c7062f`,提成独立模块 + 5 个单元测试,其中一个钉住"每个字节上线
恰好一次")。**但它不是 wedge 的原因** —— 修完 5 轮 uringgate 依然 0-13 轮内卡死。

**转折点是一行诊断,不是又一次源码通读。** 在门里加了:卡死时另开一条连接 PING。

```
uringgate: FAIL — round 13, step [bt: ZADD after the blocking timeout]: WEDGED
uringgate: triage — fresh conn to the same server: b'+PONG' => SHARD ALIVE, per-conn wedge
```

reactor 活着。于是整个 recv 侧、io_uring 侧、SQ 压力侧全部出局 —— 故障在**单连接
状态**里。再看失败步骤的分布:永远是"阻塞超时之后的那条命令",从来不是"立即命中
之后的那条命令"。**命中走唤醒路径,超时走超时路径** —— 差异就在这两条路径之间。

### 根因

阻塞命令 miss 时 `try_inline_local` 提前返回,**故意不递增 `next_emit`**(回复推迟到
解除阻塞的那条路径去发)。因此每条解除路径都有义务把这个 seq 退休。四条里三条做了:
`wake_blocked_on_key`、`block_xshard::deliver_block`、跨 shard 超时清扫。**本 shard 的
`tick_blocked_timeouts` 没做。**

于是连接永久落后一个 seq。只要后续命令都走 inline 快路径(直写 `conn.output`),
偏移就被掩盖着看不出来。**第一条走 pending 路径的命令**(跨 shard 转发,或排在别人
后面的)把回复 fold 到 `seq - next_emit` = 1 而不是 0 —— 一个从未分配的槽位。回复被
丢弃。客户端永远等一条服务端收到了、执行了、也答了的命令。

修复是一行(`8d8f20e9`),注释比代码长十倍,因为值钱的是"为什么这里必须有它"。

### 证据

| | 修复前 | 修复后 |
|---|---|---|
| uringgate,lx64 6.12.95,4 shards | 5/5 FAIL(round 0-13) | **780 轮 0 FAIL**(300 轮单跑 + 8×60) |
| uringgate,macOS/kqueue | round 0 FAIL | PASS |

macOS 那一行是关键:**这个 bug 与 reactor 无关**。`blocked.rs` 两个 reactor 共用,
epoll 和 kqueue 一样中招。它之所以看起来像"io_uring bug",只是因为唯一跑 uring 的
测试层是概率性的 gate,而 Rust 集成测试全都 `KEVY_IO_URING=0`。

门因此改成:blocking-tail 在所有 reactor 上跑,sq-pressure 才需要真 ring。**没有
io_uring 的机器现在覆盖这道门的一半,而不是零。** 已接 CI。

### 教训

- **"trace 显示 dispatch 了但回复没到" 不等于"问题在写侧"。** 回复可以在写侧之前
  就丢 —— 丢在 fold 的槽位索引上。第三幕把下一步定为"追写侧",方向对了一半:写侧
  确实有 bug,但不是这个 bug。
- **一行分诊胜过一夜通读。** "另开一条连接看看还活着吗" 一次就把搜索空间从"整个
  io_uring 数据平面"砍到"单连接状态机"。这个诊断本该在第三幕开头就做。
- **兄弟路径不对称 = 高危。** 四条解除阻塞的路径做同一件事,三条记得退休 seq,一条
  忘了。这类 bug 不会被"读这个函数"发现,只会被"把做同一件事的所有地方列出来对齐"
  发现。
- **前三幕的两个已发布修复仍然成立**(conformance 4/25 → 0/25 是独立证据),只是它们
  修的不是这个。一个症状可以有多个原因;修好一个不代表症状会消失,也不代表那个修复
  是错的。
