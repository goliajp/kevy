# FINDING(未闭):x86 io_uring 服务端在 GH hosted runner 上的确定性悬挂(2026-07-18)

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
