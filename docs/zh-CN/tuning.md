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
| 内核 `mitigations=off`        | 受信任的单租户机                     | 12–25%  |
| 清空 netfilter 规则           | 独占主机,不需要本机防火墙           | **25–35%** |
| PGO(profile-guided)          | 工作负载固定的 release build         | 1–10%   |

`mitigations=off` 和清空 netfilter 是仅有的两个能动**内核地板**的旋
钮;PGO 和其余只削用户态周期。

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

## 清空 netfilter / iptables 规则(很大,但危险)

Linux 内核每个 syscall 经过 netfilter / nftables hook —— `tcp_sendmsg`、
`tcp_recvmsg`、`__dev_queue_xmit`,**包括 loopback**。规则集复杂(docker、
libvirt、fail2ban、ufw 每个加 50-300 条规则)时,累计开销巨大。

lx64 参考机实测(Linux 6.12,`mitigations=off`,典型 docker + libvirt
+ Tailscale 规则集 ~500 条):

| 工作负载         | 规则开启(默认) | 规则清空    | Δ     |
|------------------|------------------|-------------|-------|
| C c1 SET         | 80.6 k           | **108.9 k** | +35%  |
| C c1 GET         | 80.0 k           | **108.3 k** | +35%  |
| Rust 客户端 c1   | ~77 k            | ~96 k       | +25%  |

比 `mitigations=off` 还大。

### 代价

`iptables -F` + `nft flush ruleset` 清除主机上**所有**防火墙和 NAT
规则。之后:

- **docker 端口转发坏掉**(依赖 iptables NAT)
- **libvirt VM 失去 NAT**(default virbr0 → eth0 的 MASQUERADE)
- **Tailscale / WireGuard** 失去 allow-list 规则
- **ufw / fail2ban / firewalld** 被绕过 —— 公网暴露的主机**入站流量不再过滤**

### 可接受场景

- 专用 kevy 主机,放在 VPC 后面,防火墙在 AWS SG / GCP firewall / 边界
  网关层
- 裸金属机,所有服务跑同机内,只走 UNIX socket 或 loopback
- bench / dev 机

### 不能用的场景

- 任何直接暴露公网的主机(前面没硬件防火墙)
- 多租户主机
- docker / podman 上跑别人 workload 的主机

### 怎么应用(以及回滚)

```sh
# 先备份
nft list ruleset > /tmp/nft-backup.nft
iptables-save > /tmp/iptables-backup.rules

# 清空
nft flush ruleset
iptables -F
iptables -X

# (kevy 不动而变快;如有其他服务自己验)

# 需要时回滚(比如重启 docker 前)
iptables-restore < /tmp/iptables-backup.rules
nft -f /tmp/nft-backup.nft  # xtables-compat 规则可能有警告,无害
```

更安全的方案:**保留规则但单独给 kevy 端口开通早期 ACCEPT**:

```sh
iptables -I INPUT 1 -p tcp --dport 6004 -j ACCEPT
iptables -I OUTPUT 1 -p tcp --sport 6004 -j ACCEPT
```

可以拿回大约一半的 +35%,但防火墙姿态保持完整。

## Profile-guided optimization(PGO)

工作负载固定的部署(知道读写比、命令分布、连接数),PGO 让 LLVM 用
runtime profile 数据优化二进制。lx64 实测 1-10%;`drain_inbound` 和
dispatch 循环上最大。

```sh
# Step 1: build instrumented
RUSTFLAGS="-Cprofile-generate=/tmp/pgo" cargo build --release

# Step 2: 跑代表性 workload 收 profile
LLVM_PROFILE_FILE=/tmp/pgo/kevy-%m_%p.profraw \
  ./target/release/kevy --port 6004 --no-aof &
# 另一终端跑实际生产形状的 workload ~30 秒
kill %1
sleep 3  # 让 profile data flush

# Step 3: merge
llvm_profdata=$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata
$llvm_profdata merge -o /tmp/pgo/merged.profdata /tmp/pgo/*.profraw

# Step 4: rebuild
cargo clean
RUSTFLAGS="-Cprofile-use=/tmp/pgo/merged.profdata" cargo build --release
```

需要 `rustup component add llvm-tools-preview` 拿 `llvm-profdata`。
merged.profdata 约 70 KB,可以跟源码一起 commit,只要 workload 形态
不变就一直用同一份 profile。

PGO **不在** 上游 release 里默认开 —— 它跟 workload 绑死。大部分生产
用户不会在意 1-10%;真正在意的部署照上面 recipe 自己跑。

## `io_uring` SQPOLL —— 实测拒绝接入

内核独立线程轮询 io_uring 提交队列 —— 消除每 op 一次的
`io_uring_enter` syscall。

wire-level 支持在 `kevy_uring::IoUring::new_sqpoll` 里,但**没有接入
shard reactor**,也不建议套在 kevy 的 thread-per-core 之上。每个 ring
会生一个内核轮询线程,N 个 shard = N 个额外的 100% 自旋内核线程,跟
shard 线程抢同一批核。lx64 参考机(10 shard 跑 16 核)实测在 -c1 和
-c50 上**回归 2–15×**。

SQPOLL 适合单线程 reactor + 有空余核给轮询线程的场景。kevy 的
per-core 设计已经吃满 CPU,再加一个内核轮询线程相当于砍一半。实测细
节见 `bench/PERF-ATTACK-LOG-2026-06-20.md` 里的 D5。

## 不再有用的事

- `taskset` 单核:io_uring 失去并行,反而不如 shared-nothing 分片
- 关 THP:对 kevy 的 allocator 模式没明显影响
- `numactl --interleave`:只在多 socket 才有用;lx64 单 socket
- 关 slowlog:默认就是关的(`slower-than-micros = -1`)

## 见

- [`bench/PERF-PROFILE-2026-06-20.md`](../../bench/PERF-PROFILE-2026-06-20.md) —— 引出这一页旋钮单的火焰图诊断
- [`bench/PERF-ATTACK-LOG-2026-06-20.md`](../../bench/PERF-ATTACK-LOG-2026-06-20.md) —— 每个旋钮的实测日志
- [`bench/REPORT.md`](../../bench/REPORT.md) —— 基准方法学
