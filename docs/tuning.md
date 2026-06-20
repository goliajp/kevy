# Tuning kevy for raw throughput

This page lists the levers that materially change kevy's per-op
overhead. Each has measured impact numbers (lx64, Intel Xeon 6, Linux
6.12 / io_uring; methodology in [`bench/REPORT.md`](../bench/REPORT.md))
and a clear cost. Apply only the ones you actually need.

## tl;dr

| Lever                         | When                                     | Gain    |
|-------------------------------|------------------------------------------|---------|
| Pin server to a CPU set       | dedicated host, share machine with bench | 5–15%   |
| Disable AOF (`--no-aof`)      | replica / ephemeral cache                | 5–10%   |
| Set `KEVY_IO_URING=1`         | Linux 5.13+                              | 10–30%  |
| Kernel `mitigations=off`      | trusted single-tenant box                | 12–15%  |
| `io_uring` SQPOLL (planned)   | Linux 5.13+, can spare 1 core            | 1.5–2×  |

`mitigations=off` and SQPOLL are the only knobs that move the kernel
floor; everything else trims userspace cycles only.

## CPU pinning

io_uring's reactor benefits from sticking to a fixed CPU set so the
NIC IRQ → softirq → user thread path stays on the same L1/L2:

```sh
taskset -c 0-9 kevy --port 6004
```

If you bench in the same box, **pin server and client to disjoint
core ranges** — server on `0-9`, client on `10-15` (or whatever your
topology gives). Sharing cores re-introduces scheduler ping-pong that
swamps any io_uring gain. See `feedback-kevy-bench-isolation` for the
gory details.

## `KEVY_IO_URING=1`

Switches the reactor from epoll to io_uring. Linux 5.13+ required;
older kernels silently fall back to epoll. On the lx64 box this is
worth +10–30% at -c1 and is the prerequisite for SQPOLL (D5).

```sh
KEVY_IO_URING=1 kevy --port 6004
```

## Disable AOF for replicas / caches

Default is `--aof` (durability). For a read-replica / pure cache, every
write you take is wasted disk I/O:

```sh
kevy --port 6004 --no-aof
```

Wallclock impact depends on your write rate; tail latency drop is
larger than median.

## Kernel `mitigations=off` (Spectre / BHB)

> **Read the entire section before flipping this. It is a security
> trade-off, not a free lunch.**

On Linux kernels with Spectre BHB mitigations enabled (the default
since Linux 6.x), every syscall pays for `clear_bhb_loop` — a small
in-kernel loop that flushes the branch history buffer to prevent
speculative-execution side-channel leaks across the user/kernel
boundary.

On the lx64 reference box (Intel Xeon 6, Linux 6.12), `clear_bhb_loop`
is the **single largest CPU consumer on the kevy server** during
`-c1` workloads — **13.3%** of CPU time, more than any kevy userspace
symbol. At `-c50` it drops to ~5% because the syscall is amortized
across more work per op.

### What you give up

Booting with `mitigations=off` disables hardware-vulnerability
mitigations *across the board*: Spectre v1/v2/BHB, Meltdown, MDS,
TAA, L1TF, retbleed, etc. This is **only acceptable** on:
- single-tenant boxes (you own the kernel, no untrusted code runs)
- machines isolated from the network at L3 (or behind a trusted gateway)
- benchmark / test rigs

Do **not** apply this to multi-tenant hosts, shared CI runners, or
anything that processes untrusted user code (Lua eval-from-the-wire,
embedded plugins from third parties, etc.).

### How to apply

Edit your bootloader's kernel cmdline (e.g. `/etc/default/grub`'s
`GRUB_CMDLINE_LINUX_DEFAULT`), add `mitigations=off`, regenerate:

```sh
# Debian / Ubuntu
sudo update-grub
sudo reboot
```

Verify after reboot:

```sh
cat /proc/cmdline | grep mitigations
# ... mitigations=off ...

cat /sys/devices/system/cpu/vulnerabilities/* | head
# ... should report "Vulnerable" or "Mitigation: ..." disabled
```

### Measured gain

On the lx64 reference, expected throughput delta after `mitigations=off`:

| Workload    | Rust client -c1 | C `redis-benchmark` -c1 |
|-------------|-----------------|--------------------------|
| Before      | ~65 k ops/s     | ~67 k ops/s              |
| After (pred)| ~75 k ops/s     | ~78 k ops/s              |

(Numbers are kernel/CPU dependent. AMD Zen 3+ pays a different price
than Intel Xeon Spectre BHB; ARM N1/N2 pay yet another. Measure on
your hardware.)

## `io_uring` SQPOLL (planned, not yet shipped)

Kernel polls the io_uring submission queue from a dedicated thread —
eliminates `io_uring_enter` syscall per op. Will be an opt-in feature
flag (`KEVY_SQPOLL=1`), since it costs 1 CPU core at 100% even when
idle. Predicted gain: **1.5–2×** at -c1, neutral at -c50 (already
batched).

Status: tracked as D5 in `bench/PERF-ATTACK-LOG-2026-06-20.md`.

## Things that do **not** help (anymore)

- `taskset` to single core: io_uring loses parallelism, slower than
  shared-nothing shard layout
- Disabling THP: no measurable effect on kevy's allocator pattern
- `numactl --interleave`: only matters on multi-socket; lx64 is single-socket
- Disabling slowlog: already off by default (`slower-than-micros = -1`)

## See also

- [`bench/PERF-PROFILE-2026-06-20.md`](../bench/PERF-PROFILE-2026-06-20.md) — flamegraph diagnosis that motivated this knob list
- [`bench/PERF-ATTACK-LOG-2026-06-20.md`](../bench/PERF-ATTACK-LOG-2026-06-20.md) — per-lever measurement log
- [`bench/REPORT.md`](../bench/REPORT.md) — benchmark methodology
