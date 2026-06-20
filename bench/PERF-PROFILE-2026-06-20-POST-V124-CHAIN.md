# Perf profile, 2026-06-20 (post-v1.24 chain), lx64

Re-diagnosis after shipping the v1.24.x autorun chain
(E14 / A2 / A3 / A9 / A5 / A6+A7 / C6 / B4). Confirms where the
bottleneck moved + sets future-session direction.

## Bench

- lx64 16-core, kernel 6.12, mitigations=off (from session 8).
- `release-perf` profile (LTO + 1 cgu + abort + line-tables-only +
  force-frame-pointers).
- `taskset -c 0-9` for server; `taskset -c 10` for client.
- `redis-benchmark -c 1 -P 1 -n 1.5M -t get,set`.

**Result**: SET **82562** / GET **82891** rps. C client baseline
on the same lx64 was ~82 k both → **kevy Rust c1 has matched C
client parity**. The v1.23.0 SHIPPED headline (84 k SET/GET) is
reproduced consistently across runs; B4 added c100 SET 188 k.

## Top self-time (perf record -F 999 --call-graph fp, 7 s)

| % self | symbol                                                       |
|--------|--------------------------------------------------------------|
| 51.66% | `kevy_rt::runtime::Runtime::run::closure`                    |
|  4.64% | `libc.so.6 syscall`                                          |
|  4.27% | `entry_SYSRETQ_unsafe_stack` (kernel)                        |
|  3.59% | `kevy_rt::uring_inbox::Shard::uring_drain_inbound`           |
|  2.96% | `arch_exit_to_user_mode_prepare` (kernel)                    |
|  2.01% | `entry_SYSCALL_64_after_hwframe` (kernel)                    |
|  1.85% | `do_syscall_64` (kernel)                                     |
|  1.80% | `__do_sys_io_uring_enter` (kernel)                           |
|  1.37% | `syscall_return_via_sysret` (kernel)                         |
|  1.26% | **`nft_do_chain` (kernel)** ⚠ netfilter on loopback!         |
|  1.03% | `entry_SYSCALL_64` (kernel)                                  |
|  0.94% | `entry_SYSCALL_64_safe_stack` (kernel)                       |
|  0.88% | `kevy_rt::shard_flush::Shard::flush_wakes`                   |
|  0.85% | `<core::iter::adapters::map::Map>::next`                     |
|  0.83% | `syscall_exit_to_user_mode` (kernel)                         |
|  0.76% | `kevy_rt::shard_flush::Shard::flush_backlog`                 |
|  0.50% | `libc.so.6 malloc`                                           |

## Top callgraph (children)

| % | symbol                                                            |
|---|-------------------------------------------------------------------|
| 52.08% | `Runtime::run::closure` (host of everything below)            |
| 38.56% | `libc.so.6 syscall` (all kinds, inclusive)                    |
| 14.10% | `__do_sys_io_uring_enter`                                     |
| 13.65% | `io_issue_sqe`                                                |
| 12.21% | `io_submit_sqes`                                              |
| 11.57% | `io_write`                                                    |
| 11.38% | `sock_write_iter`                                             |
| 11.14% | `tcp_sendmsg`                                                 |
| 11.00% | `tcp_sendmsg_locked`                                          |
|  9.78% | `tcp_write_xmit` / `__tcp_push_pending_frames`                |
|  9.44% | `__tcp_transmit_skb`                                          |
|  9.03% | `__ip_queue_xmit`                                             |

## Interpretation

- **51% in `Runtime::run::closure`** — the busy-poll reactor body
  is dominantly inlined into one symbol. Per-callsite optimizations
  (A2/A3/A9/A5/A6+A7) all rolled up here. Sub-attacking inside
  this body returns < 1 pp per change at -c1.
- **38% inclusive in syscalls** — even with E14's threshold-based
  enter skip the write-side syscall path is the largest single
  CPU bucket. `tcp_sendmsg` + the IP/skb transmit stack dominate
  (~11% each).
- **1.26% in `nft_do_chain`** — every loopback packet traverses
  netfilter chains (ts-input / LIBVIRT_INP / DOCKER on this box).
  Direct, attackable on the deployment side via per-port fast-path
  ACCEPT.
- **0.85% in `Map::next`** — the cross-shard fan-out helpers
  (`flush_wakes` 0.88%, `flush_backlog` 0.76%) iterate per shard.
  Cleanup-able but small.
- **0.50% in `malloc`** — A5's InlineRanges removed the per-request
  argv-range vec alloc; remaining mallocs are smaller. Argv pool +
  Conn buffer growth account for most of it.

## What this means for further attacks

**Userspace ceiling reached** at the redis-benchmark single-conn
RTT workload. Further -c1 gains require attacking one of:

1. **Kernel side (D-series deployment)**.
   - D9: per-port iptables/nftables fast-path ACCEPT. **Direct
     1.2-2% win** attributable to the `nft_do_chain` line above.
     Pure config; no kevy code change.
   - D1: hugetlbfs for .text. Reduces iTLB miss inside
     `Runtime::run::closure`. Harder to set up; requires
     hugepage-reserved memory + binary wrapper.

2. **Larger architectural restructure**.
   - A4 AoS → SoA Conn — split hot vs cold Conn fields, attacks
     L1D-miss inside the closure. Invasive — every conn field
     access changes.
   - A1 split run_uring main loop — readability + lets LLVM choose
     better spill / register allocation per phase. Diagnostic
     benefit (each phase shows separately in perf report) > perf
     benefit.
   - A14 PubSub RCU — only matters under pubsub workload, not -c1
     GET/SET.

3. **Bigger workloads + amortising features**.
   - B5 MSG_ZEROCOPY — attacks `tcp_sendmsg` (11% inclusive). Only
     wins when reply payload > ~ 4 KB; not the redis-benchmark
     workload.
   - B6 IORING_REGISTER_BUFFERS — same workload caveat.

**Recommendation**: stop chasing -c1 in userspace; either ship
v1.24.0 with what's already on develop (matches v1.23.0 headline
+ B4 c100 boost + 9 architectural improvements) or pivot to
kernel/deployment side (D-series) and bench bigger workloads.

## What was tried + dropped this autorun cycle

- **A11** IORING_SETUP_TASKRUN_FLAG — race-y vs busy-poll loop;
  30% c1 GET regression. Reverted; in-code rationale stays in
  `ring.rs::submit_and_wait`.
- **C2 AutoFDO** — deferred (LLVM tooling chain > autorun-item
  scope).
- **A8 Slab allocator** — KevyMap slot array already IS the slab.
- **B1 E13 propagate** — already covered by `KevyMap::alloc_table`.
- **B2 HashMap audit** — 49 usages all in cold control planes.
