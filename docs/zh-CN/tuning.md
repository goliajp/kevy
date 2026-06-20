# 调优 kevy 拿到极限吞吐

本页列出会显著改变 kevy 单 op 开销的调优旋钮。每个旋钮都给了实测影响
(lx64,Intel Xeon 6,Linux 6.12 / io_uring;方法学见
[`bench/REPORT.md`](../../bench/REPORT.md))和明确的代价。只挑你确实需
要的那几条。

## 速查

| 旋钮                          | 适用                                | 收益    |
|-------------------------------|-------------------------------------|---------|
| 把 server 钉在固定 CPU 集     | 独占主机,或同机跑 bench             | 5–15%   |
| 关 AOF (`--no-aof`)           | 只读副本 / 易失缓存                  | 5–10%   |
| 打开 `KEVY_IO_URING=1`        | Linux 5.13+                          | 10–30%  |
| 内核 `mitigations=off`        | 受信任的单租户机                     | 12–15%  |
| `io_uring` SQPOLL (计划中)    | Linux 5.13+,能让一核满转            | 1.5–2×  |

`mitigations=off` 和 SQPOLL 是仅有的两个能动**内核地板**的旋钮;其余
只削用户态周期。

## CPU 绑定

io_uring reactor 钉在固定 CPU 集上跑得最稳 —— 网卡 IRQ → softirq →
用户线程一路保持在同一颗 L1/L2:

```sh
taskset -c 0-9 kevy --port 6004
```

如果你在同一台机器上跑 bench,**server 和 client 必须绑到不相交的核段**
—— server `0-9`,client `10-15`(看具体拓扑)。共核会让调度器抢占抵消
掉 io_uring 的所有收益。细节见 `feedback-kevy-bench-isolation`。

## `KEVY_IO_URING=1`

把 reactor 从 epoll 换到 io_uring。需要 Linux 5.13+,老内核会静默回退
到 epoll。lx64 实测 -c1 +10–30%,也是 SQPOLL (D5) 的前置。

```sh
KEVY_IO_URING=1 kevy --port 6004
```

## 副本 / 缓存模式关 AOF

默认 `--aof`(持久化)。如果是只读副本或纯缓存,每次写都是浪费的磁盘
I/O:

```sh
kevy --port 6004 --no-aof
```

吞吐影响看你的写比例;**尾延迟下降比中位数明显**。

## 内核 `mitigations=off`(Spectre / BHB)

> **整段读完再决定要不要动。这是安全 trade-off,不是免费午餐。**

Linux 6.x 起默认开 Spectre BHB 缓解,每次 syscall 都要走一遍
`clear_bhb_loop` —— 一段内核里的小循环,刷分支历史缓存,防止跨用户/
内核态边界的推测执行侧信道泄露。

lx64 参考机(Intel Xeon 6,Linux 6.12)上,`clear_bhb_loop` 是 kevy
server `-c1` 工作负载下**单一最大的 CPU 消费者** —— **13.3%**,超过任何
kevy 用户态 symbol。`-c50` 下降到约 5%,因为 syscall 被批量摊薄了。

### 你要放弃什么

启动加 `mitigations=off` 等于**全面关掉**硬件漏洞缓解:Spectre v1/v2/
BHB、Meltdown、MDS、TAA、L1TF、retbleed 等全没。**只能用在:**
- 单租户机(内核自己控,不跑不受信任的代码)
- 网络 L3 隔离(或在受信任网关后)
- bench / 测试机

**不要**用在多租户主机、共享 CI runner、或会跑不受信任用户代码的场景
(从网线吃 Lua eval、加载第三方插件等)。

### 怎么打开

改 bootloader 内核 cmdline(比如 `/etc/default/grub` 的
`GRUB_CMDLINE_LINUX_DEFAULT`),加 `mitigations=off`,重新生成:

```sh
# Debian / Ubuntu
sudo update-grub
sudo reboot
```

重启后核实:

```sh
cat /proc/cmdline | grep mitigations
# ... mitigations=off ...

cat /sys/devices/system/cpu/vulnerabilities/* | head
# ... 应该报 "Vulnerable" 或 "Mitigation: ..." 已禁
```

### 实测收益

lx64 参考机上,`mitigations=off` 后预期吞吐:

| 负载        | Rust 客户端 -c1 | C `redis-benchmark` -c1 |
|-------------|-----------------|--------------------------|
| 关前        | ~65 k ops/s     | ~67 k ops/s              |
| 关后(预测) | ~75 k ops/s     | ~78 k ops/s              |

(数字看内核 / CPU 厂家。AMD Zen 3+ 跟 Intel Xeon BHB 的代价不同;
ARM N1/N2 又是另一回事。**在你自己的硬件上量**。)

## `io_uring` SQPOLL(计划中,未发布)

内核独立线程轮询 io_uring 提交队列 —— 消除每 op 一次的
`io_uring_enter` syscall。会做成可选 feature flag(`KEVY_SQPOLL=1`),
因为它就算闲着也要占满一个核。预测 -c1 **1.5–2×**,-c50 持平(已经
batch 过)。

进度:见 `bench/PERF-ATTACK-LOG-2026-06-20.md` 里的 D5。

## 不再有用的事

- `taskset` 单核:io_uring 失去并行,反而不如 shared-nothing 分片
- 关 THP:对 kevy 的 allocator 模式没明显影响
- `numactl --interleave`:只在多 socket 才有用;lx64 单 socket
- 关 slowlog:默认就是关的(`slower-than-micros = -1`)

## 见

- [`bench/PERF-PROFILE-2026-06-20.md`](../../bench/PERF-PROFILE-2026-06-20.md) —— 引出这一页旋钮单的火焰图诊断
- [`bench/PERF-ATTACK-LOG-2026-06-20.md`](../../bench/PERF-ATTACK-LOG-2026-06-20.md) —— 每个旋钮的实测日志
- [`bench/REPORT.md`](../../bench/REPORT.md) —— 基准方法学
