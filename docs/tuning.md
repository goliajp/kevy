# Tuning kevy

A reference for the runtime knobs that change kevy's per-op cost — CPU layout, reactor choice, persistence, memory limits, network transport, and a few Linux-side levers.

## When you need this

Reach for this doc when:

- A benchmark is showing kevy under your throughput or latency target and you want to know which knob to turn next.
- You are deploying kevy on a host where the defaults (TCP loopback, io_uring auto-detect, `appendfsync everysec`, no `maxmemory`) do not match the workload — e.g. sparse-conn services, NVMe-backed durability requirements, or memory-capped cache tiers.
- You are profiling kevy with `perf` and need the build profile that keeps debug line tables on.

If you are just starting kevy on a laptop and the numbers look fine, you do not need this page. Defaults are tuned to be reasonable across workloads.

## Core idea

kevy is a thread-per-core server: one shard per OS thread, shared-nothing keyspace partitioned by CRC16 hashtag, busy-poll reactor on each shard. The defaults aim at "decent on every workload"; tuning means matching the shard count, the reactor, and the persistence policy to **what your perf data actually shows is the bottleneck**. Do not pre-emptively flip knobs. Measure, identify the cost, then change one variable at a time.

## Tuning playbook

### CPU and shards

| Knob | Where | Default | Effect |
|------|-------|---------|--------|
| `--threads N` / `KEVY_THREADS` | CLI / env | number of online cores | shard count; one OS thread per shard |
| `--accept-shards K` | CLI | all shards accept | only the first K shards bind a listener; the rest are compute-only |
| CPU pinning | `taskset` / `numactl` | none | locks shards to a fixed core set |

**Picking `--threads`.** Set this to the parallelism actually present in the workload. A single-client pipelined benchmark (`-c 1 -P 16`) saturates one shard; setting `--threads 10` here makes nine shards busy-poll for no work and steal cache lines from shard 0. For real multi-client workloads, start at `min(cores, expected concurrent clients / 4)` and measure.

**Picking `--accept-shards`.** When the connection-to-shard ratio is low (sparse-conn workloads — say, 50 clients across 10 shards = 5 conns/shard), the per-iteration busy-poll overhead stops amortizing and throughput drops. The rule of thumb is `ceil(conns / 20)` — for 50 conns, set `--accept-shards 3` and let three listening shards each take roughly 17 connections while the remaining shards stay compute-only and still receive cross-shard work via the internal dispatcher. The empirical sweet spot is broader than the point estimate; see [docs/accept-shards.md](https://github.com/goliajp/kevy/blob/develop/docs/accept-shards.md) for the full sweep and a discussion of when the cross-shard hop tax outweighs the accept-concentration win.

**CPU pinning.** On a benchmark or single-tenant host, pinning kevy to a fixed core set keeps the NIC IRQ → softirq → user-thread path on the same L1/L2:

```sh
taskset -c 0-9 kevy --port 6004 --threads 10
```

If the client runs on the same machine, pin server and client to **disjoint** core ranges (server `0-9`, client `10-15`). Shared cores reintroduce scheduler ping-pong that swamps any reactor gain.

### Reactor choice

| Platform | Default | Override |
|----------|---------|----------|
| Linux ≥ 5.19 | io_uring (auto-detected) | `KEVY_IO_URING=0` forces epoll; `KEVY_IO_URING=1` requires io_uring and exits loudly if `io_uring_setup` is blocked by seccomp |
| macOS / *BSD | kqueue | not configurable |
| Older Linux | epoll | n/a |

The Linux auto-detect runs `io_uring_setup` at startup; if the syscall is blocked (seccomp profile, locked-down container) kevy silently falls back to epoll. In a hardened deployment that you *want* to fail loudly rather than silently degrade, set `KEVY_IO_URING=1` so the server refuses to start unless io_uring is actually available. Conversely, when you need to take io_uring out of the picture for a reproducible epoll-vs-io_uring benchmark or to work around a kernel regression, set `KEVY_IO_URING=0`.

```sh
KEVY_IO_URING=1 kevy --port 6004   # require io_uring, exit if blocked
KEVY_IO_URING=0 kevy --port 6004   # force epoll
```

### Persistence

AOF policy is controlled by `appendfsync` (config file or `CONFIG SET`). The three values match Redis semantics:

| `appendfsync` | Durability | Cost |
|---------------|------------|------|
| `always` | every write `fsync`-ed before reply | highest latency; bounded by NVMe sync latency |
| `everysec` (default) | `fsync` once per second on a background thread | bounded data loss window of 1 s; near-zero hot-path cost |
| `no` | never `fsync`; kernel flushes on its own schedule | fastest; data loss window = page-cache flush interval |

The background `fsync` for `everysec` runs on a dedicated bio thread off the shard hot path, so shard tail latency is not coupled to disk latency. For a pure cache or a read-replica, also consider disabling AOF entirely with `--no-aof` (no AOF file is written at all, not even buffered).

### Memory

| Knob | Default | What it does |
|------|---------|--------------|
| `maxmemory` | unlimited | hard memory cap in bytes; once reached, the eviction policy kicks in |
| `maxmemory-policy` | `noeviction` | which keys to drop when the cap is hit |
| `maxmemory-samples` | 5 | sample size for the approximate-LRU/LFU policies |

Eviction policies mirror Redis: `noeviction`, `allkeys-lru`, `allkeys-lfu`, `allkeys-random`, `volatile-lru`, `volatile-lfu`, `volatile-random`, `volatile-ttl`. `noeviction` makes writes fail with OOM once the cap is hit and is the safe default for a primary store; the `allkeys-*` policies are correct for a cache tier where any key is disposable.

`maxmemory-samples` is a quality-vs-cost dial for the approximate policies — sampling more keys produces a closer approximation to true LRU/LFU at a per-eviction CPU cost. The default of 5 is sufficient for most cache workloads; raise to 10 if you can see eviction picking poor victims in your access pattern, lower to 3 only if eviction itself is showing up in profiles.

### Network

The default transport is TCP. When the client lives on the same host, switch to a Unix-domain socket and skip the loopback TCP stack entirely:

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
redis-cli -s /tmp/kevy.sock SET foo bar
```

The server dual-binds: TCP stays available for remote clients, UDS handles local ones. Same RESP semantics, same shard runtime. The gain on local-client workloads is large (the loopback TCP path is the dominant cost at small payload sizes); see [docs/uds.md](https://github.com/goliajp/kevy/blob/develop/docs/uds.md) for the full numbers, the permissions model, and the cases where UDS does not apply.

**Bind address warning.** kevy has no AUTH and no TLS today. Binding to a non-loopback address (`--bind 0.0.0.0` or any public interface) prints a startup warning, because anything on the network can then issue commands. Run kevy behind a private network boundary or behind a proxy that terminates auth.

### Linux kernel knobs

Two host-level levers move the kernel floor that sits underneath kevy. Both are benchmark / single-tenant-only — read the trade-offs before applying.

**Spectre / BHB mitigations.** On Linux 6.x kernels with mitigations enabled (the default), every syscall pays for `clear_bhb_loop` and friends. On a small-payload `-c 1` workload this is the single largest CPU consumer in a kevy run. Disabling mitigations at the kernel cmdline:

```sh
# Add `mitigations=off` to GRUB_CMDLINE_LINUX_DEFAULT, then:
sudo update-grub && sudo reboot
cat /proc/cmdline | grep mitigations
```

is only acceptable on single-tenant boxes where no untrusted code runs (no Lua-from-the-wire, no third-party plugins, no multi-tenant containers). Do not apply to multi-tenant hosts, shared CI runners, or anything processing untrusted user code. The gain is in the +10–15% range on `-c 1`, smaller as the workload pipelines more.

**Hugepages for the `.text` segment.** kevy can call `madvise(MADV_HUGEPAGE)` on its own code segment, which lets the kernel back the kevy binary's instructions with 2 MiB pages instead of 4 KiB. The win is a smaller iTLB footprint on the hot dispatch loop. This costs effectively nothing at runtime and is worth enabling on Linux hosts where `/sys/kernel/mm/transparent_hugepage/enabled` is `always` or `madvise`. The trade-off is purely the small one-time cost of the `madvise` call at startup; there is no security trade-off, unlike `mitigations=off`.

## Profiling

For a `perf record` flamegraph that resolves to actual symbols, build with the `release-perf` profile — same optimization level as `release` but with debug line tables retained:

```sh
cargo build --profile release-perf
./target/release-perf/kevy --port 6004 --threads 1 &
KEVY_PID=$!

perf record -F 999 -p $KEVY_PID -g --call-graph=fp -- sleep 30
perf report --stdio | head -60

# Resolve raw addresses for inlined symbols:
addr2line -e ./target/release-perf/kevy -f -i 0x<addr>
```

The standard `release` profile strips line tables, so `perf` reports raw addresses with no symbols and `addr2line` returns `??`. Don't profile a `release` binary; rebuild with `release-perf` first.

For symbol-level attribution of `clear_bhb_loop` and other kernel-side cost, capture with `--call-graph=dwarf` instead of `fp` and use the same `addr2line` flow. The dwarf unwinder is slower but unwinds across the syscall boundary correctly.

## Trade-offs

| Knob | Costs | Buys |
|------|-------|------|
| `--threads N` (raise) | wasted CPU on idle busy-poll shards if N > workload parallelism | more concurrent client capacity |
| `--threads N` (lower) | one shard's worth of cross-shard hop tax avoided | less wasted CPU on sparse-conn workloads |
| `--accept-shards K` | listener concentration; fewer entry points if clients connect via raw `connect` | per-iter overhead amortizes across more conns on each accepting shard |
| `KEVY_IO_URING=1` (force) | server refuses to start when seccomp blocks io_uring | no silent degradation to epoll on hardened hosts |
| `KEVY_IO_URING=0` (force epoll) | gives up io_uring's per-op saving | reproducible epoll baseline; works around kernel regressions |
| `appendfsync always` | every write blocks on `fsync` | zero-data-loss durability |
| `appendfsync no` | data loss window = page-cache flush interval | fastest write path |
| `--no-aof` | no persistence at all | minimum disk I/O; useful for replicas / caches |
| `maxmemory` set | writes can fail (`noeviction`) or evict (`allkeys-*`) | bounded memory footprint |
| `maxmemory-samples` raise | per-eviction CPU cost | better approximate-LRU/LFU victim choice |
| Unix-domain socket | local-only; filesystem-permission security model | skips the TCP loopback stack |
| `mitigations=off` | Spectre / Meltdown / MDS / etc. mitigations all off | reclaims the syscall-path tax |
| `MADV_HUGEPAGE` on `.text` | none meaningful | smaller iTLB footprint on the dispatch loop |
| `release-perf` build | larger binary (debug line tables) | `perf` resolves to symbols |

## FAQ

**Should I always set `--accept-shards`?**

No. The knob exists for sparse-conn workloads where conns/shards is low and the busy-poll body fails to amortize. For dense-conn workloads (say, 1000 clients on 10 shards = 100 conns/shard), the default — every shard accepts — is correct, because spreading the listener evenly reduces accept-side contention. Apply `ceil(conns / 20)` only when you actually have a sparse-conn case.

**Is io_uring always faster than epoll?**

On Linux ≥ 5.19 with a workload that batches submissions, yes, materially. On older kernels, on kernels with seccomp filters that block `io_uring_setup`, or on workloads dominated by a single syscall per op with no batching opportunity, the difference shrinks. Auto-detect is the right default; override only when you have a measured reason or a hardened deployment that should fail loudly rather than silently fall back.

**What's the production sweet spot for `appendfsync`?**

`everysec` for almost everyone. It bounds data loss to one second, runs the `fsync` off the hot path, and has near-zero impact on tail latency. Use `always` only when your durability story actually requires zero data loss (and accept that NVMe `fsync` latency now bounds your tail latency). Use `no` only for pure caches where the AOF exists just for warm-restart speed.

**When do I need `MADV_HUGEPAGE`?**

When `perf` shows iTLB misses on the hot dispatch loop, or when the host's `/sys/kernel/mm/transparent_hugepage/enabled` is set to `madvise` (in which case nothing else opts kevy in). It's a no-cost knob on Linux hosts where THP is enabled at all, so the default position is "leave it on." There is no equivalent on macOS / BSD.

**My `perf` report is full of raw addresses. What did I do wrong?**

You profiled a `cargo build --release` binary. The standard release profile strips debug line tables, so `perf` and `addr2line` have nothing to resolve against. Rebuild with `cargo build --profile release-perf` and re-record.
