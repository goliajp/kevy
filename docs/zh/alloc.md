# `kevy-alloc`——服务端的分配器

从 7.0 起，kevy 服务端不再用系统分配器（Linux 上是 glibc malloc），改用 kevy 自己的纯 Rust span 分配器 `kevy-alloc`。它是 `kevy` crate 的 `kevy-alloc` feature，默认开启，所以每一种服务端构建都带着它：`cargo install kevy`、发布页的二进制、容器镜像、`npm install -g @goliapkg/kevy-bin`。

只有服务端二进制会把它装成全局分配器。把 `kevy` crate 当库用的程序，仍然用它自己声明的分配器；链接这个 crate 不会改变它的分配方式。

## 它做什么

- **每个 shard 一个堆，快路径上没有锁。** shard 从自己线程局部的堆里分配。别的线程释放的块会送回拥有它的那个堆。
- **块没有头部。** Rust 每次释放都会给出大小，所以分配器从地址就能算出块所在的 span 和尺寸档，块旁边什么都不存。
- **页会还给操作系统。** 占用情况记在数据页之外的位图里，所以任何一个没有活块的 4 KiB 页都能单独归还（`madvise(MADV_DONTNEED)`），相邻的页照常使用。每个 shard tick 归还空闲了一段时间的页。glibc 的主 arena 只能从顶端收缩：活块下面的空闲块一直驻留在内存里。
- **内存整理。** 降级或删除的值会在原处留下空洞。当某个 shard 的 span 内空闲空间相对活数据偏大（超过活数据的 1/64，且超过 4 MiB）时，shard tick 会把分配器指出的值复制到更密的 span 里：每个 tick 0.5 ms，空闲空间超过 1/16 时最多 2 ms；同一个 tick 里的回收再把腾空的页还掉。空闲空间降到 1/256 以下就停。store 挪不动的值（索引叶子、大集合）留在原处。

## 实测

**吞吐。** firefly（aarch64 Linux，4 KiB 页），同一个 commit 构建两份，服务端绑 2 个核，`redis-benchmark -c 60`，两个构建轮换跑五轮。相对 glibc 的变化：

| 命令 | 吞吐 |
|---|---:|
| `LPUSH` | +11.3 % |
| `ZADD` | +11.4 % |
| `HSET` | +3.1 % |
| `SADD` | +1.4 % |
| `GET`、`SET` | 持平 |
| `INCR` | −2.2 % |

`INCR` 这一项是噪声：精确的指令计数（callgrind，lx64，x86-64 Linux）显示每条命令在 kevy-alloc 上是 2015 条指令，在 glibc 上是 2016 条。按同样的计数，每一个写命令在 kevy-alloc 上的指令数都不多于 glibc。

**内存。** 分层存储的服务端把常驻内存控制在预算 × 1.05 以内（见 [tiering.md](tiering.md)）。在 D1 负载上（lx64，一千万个约 1 KiB 的 hash，3 GiB 预算，两个编译好的索引），glibc 会留下约占预算 3.5 % 的空洞还不回去，整理过程把它们压紧并归还。

## 能看到什么

`INFO modules` 给出进程实际使用的分配器：

```
module:name=alloc,impl=kevy-alloc
```

`INFO allocator` 把分配器映射的每一个字节分到有名字的几项里，按各 shard 的堆求和：`alloc_live`（在用）、`alloc_rounding`（尺寸档取整）、`alloc_cache`、`alloc_span_free`（在用 span 里的空闲槽位，也就是整理要压紧的部分）、`alloc_returned`（已还给操作系统）、`alloc_virgin`（已映射、从未触碰）、`alloc_hysteresis`（已腾空、留着复用）和 `alloc_segment_overhead`。`alloc_accounted` 是它们的和，等于 `alloc_mapped`。

分层存储的内存守卫读这些数字，不去遍历系统堆，也没有堆需要 trim：`INFO tiering` 里的 `heap_trims_total` 一直是 0。

## 不带它构建

```
cargo build --release -p kevy --bin kevy --no-default-features
cargo install kevy --no-default-features
```

`kevy-alloc` 是这个 crate 唯一的默认 feature，所以这样构建只有分配器不同。它报告 `module:name=alloc,impl=system`，没有 `# Allocator` 这一节；分层存储的守卫读 glibc 的 `mallinfo2`（macOS 上读 malloc zone 统计），空闲内存堆积时用 `malloc_trim` 收缩堆。

需要这个构建的情况：

- **挂钩 malloc 的工具。** `LD_PRELOAD` 进来的分配器（jemalloc、tcmalloc）、glibc 的 `MALLOC_*` 调优参数、heaptrack 和 valgrind 的 memcheck 看的都是 malloc 调用。默认构建里服务端的分配不经过 malloc，这些工具几乎什么都看不到。
- **页大于 4 KiB 的系统。** 分配器按 4 KiB 归还页，系统页是其他大小时它一页都不还：Apple Silicon 上的 macOS（16 KiB），以及用 16 或 64 KiB 页编译的 arm64 Linux 内核。在这些系统上它仍然复用释放掉的内存，但不会还给操作系统。
- **做对比。** 想拿自己的负载和系统分配器比较，就两份都构建，在同一台机器上跑。
