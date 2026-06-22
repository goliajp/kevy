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
| Use `--threads 1`             | single client / pipelined workload       | **5–60%** |
| Switch to **Unix-domain socket** | client is same-host                   | **60–75%** |
| Kernel `mitigations=off`      | trusted single-tenant box                | 12–25%  |
| Empty netfilter ruleset       | dedicated host, no firewall needed       | **25–35%** |
| PGO (profile-guided optimize) | release build for known workload         | 1–10%   |

`mitigations=off` and emptying netfilter are the two big knobs that
move the kernel floor; UDS removes the loopback floor entirely;
`--threads` matches the shard count to your workload's parallelism;
PGO and the other userspace knobs trim userspace cycles only.

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

## Unix-domain socket (UDS) for local clients

When the client lives on the same host as the server, point it at a
filesystem socket and skip the TCP loopback stack entirely. v1.25+.

```sh
KEVY_UNIX_SOCKET=/tmp/kevy.sock kevy --port 6004
redis-cli -s /tmp/kevy.sock SET foo bar
redis-benchmark -s /tmp/kevy.sock -t set,get -n 100000 -c 50 -P 16
```

The server **dual-binds** TCP + UDS — TCP stays available for remote
clients, UDS handles local ones. Same RESP semantics, same shard
runtime. Measured impact on lx64 (precision bench, kevy 1.25, same
binary, same client process, only the address changes):

| workload | TCP rps | UDS rps | gain |
|----------|--------:|--------:|-----:|
| -c1 SET | 94.7 k | 166 k | **+76 %** |
| -c1 GET | 97.3 k | 168 k | **+73 %** |
| -c50 -P16 SET | 2.59 M | 4.11 M | **+59 %** |
| -c50 -P16 GET | 2.67 M | 4.35 M | **+63 %** |

Caveats: UDS permissions are filesystem permissions; the default
`chmod 0777` matches valkey/redis. Tighten via a containing directory
when the server box has untrusted users. Full reference (security
caveats, valkey-side equivalent, when not to use it):
[`docs/uds.md`](uds.md).

## `--threads` — shard count vs workload parallelism

kevy is thread-per-core. `--threads N` (or `KEVY_THREADS`) creates
N shards; the keyspace is partitioned by CRC16 hashtag. There is no
"more threads = always faster" — pick by workload shape:

| workload | recommendation | why |
|----------|----------------|-----|
| Single-conn benchmarks (`-c1 -P1`) | `--threads 1` | one conn pins to one shard; idle shards waste CPU on busy-poll |
| Pipelined single-client (`-c50 -P16`) | `--threads 1` | one client core can already saturate one shard; multi-shard adds cross-shard tax |
| Many independent clients, low pipelining | `--threads ≤ cores/2` | clients fan out across shards; one shard per client core |
| Mixed (cache + cluster reads) | `--threads = cores - 2` | leave headroom for the OS / IRQs |

The v1.25 precision-bench headline numbers all use `--threads 1` —
that's the configuration where redis-benchmark's per-client work hits
ceiling. Setting `--threads 10` for the same `-c1` workload **lowers**
throughput because 9 shards busy-poll for no work and steal cache
lines from shard 0.

For the multi-shard cross-routing details (`{hashtag}` slots, cluster
ports, `ClusterClient`), see [`docs/cluster.md`](cluster.md).

## BGSAVE / BGREWRITEAOF off-shard via the bio thread (v1.25)

v1.25 moved snapshot + AOF rewrite onto a **single global background
thread** (the "bio thread") — one for the whole server, not per
shard. The shards `Op::Save`-queue the request and keep busy-polling
the network; the bio thread executes the disk write off the hot path.

Net effect: the shard's busy-poll cadence is no longer interrupted by
multi-second disk writes, so tail latency under a large `BGSAVE`
drops sharply (v1.25 precision: p999 -8 %, max -18 % at c=50,
value=10 KB). No tunable — it's always on. The `--no-aof` knob still
applies if you want no AOF at all; the bio thread only runs when
there's actual disk work.

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

## Empty the netfilter / iptables ruleset (huge, but careful)

Linux kernel runs every packet through netfilter / nftables hooks
*on every syscall path* — `tcp_sendmsg`, `tcp_recvmsg`, `__dev_queue_xmit`,
even loopback. When the ruleset is non-trivial (docker, libvirt, fail2ban,
ufw, etc. each add 50-300 rules), the cumulative overhead is enormous.

Measured on the lx64 reference (Linux 6.12, `mitigations=off`, with a
typical docker + libvirt + Tailscale ruleset of ~500 rules total):

| Workload         | rules on (default) | rules empty | Δ     |
|------------------|--------------------|-------------|-------|
| C c1 SET         | 80.6 k             | **108.9 k** | +35%  |
| C c1 GET         | 80.0 k             | **108.3 k** | +35%  |
| Rust client c1   | ~77 k              | ~96 k       | +25%  |

That's a *bigger* win than `mitigations=off`.

### What you give up

`iptables -F` + `nft flush ruleset` removes **every** firewall rule and
NAT rule on the host. After this:

- **Docker port-forwarding breaks** (it relies on iptables NAT rules).
  Containers can't expose ports to the host network. Existing
  connections die.
- **libvirt VMs lose NAT** (the `default` virbr0 → eth0 MASQUERADE).
- **Tailscale / WireGuard** lose any allow-list rules.
- **ufw / fail2ban / firewalld** are bypassed. If this host is exposed
  to the internet, **incoming traffic is no longer filtered**.

### Where this is acceptable

- A dedicated kevy host inside a VPC where firewalling happens at the
  AWS Security Group / GCP firewall / on-prem perimeter layer
- A bare-metal box with all services running inside the same machine
  via UNIX sockets or loopback only
- A benchmark / dev box

### Where this is NOT acceptable

- Any host exposed directly to the public internet without a hardware
  firewall in front
- Multi-tenant boxes
- Hosts where docker / podman is running other tenants' workloads

### How to apply (and roll back)

```sh
# Backup first
nft list ruleset > /tmp/nft-backup.nft
iptables-save > /tmp/iptables-backup.rules

# Flush
nft flush ruleset
iptables -F
iptables -X

# (kevy stays up and gets faster; verify your other services if any)

# Roll back when needed (e.g., before restarting docker)
iptables-restore < /tmp/iptables-backup.rules
nft -f /tmp/nft-backup.nft  # may warn on xtables-compat rules; harmless
```

A safer alternative: keep the rules but **bypass them for the
kevy port** by adding an early `ACCEPT` at the top of the relevant
chains. The gain is smaller (you still pay one rule lookup) but the
firewall posture stays intact:

```sh
iptables -I INPUT 1 -p tcp --dport 6004 -j ACCEPT
iptables -I OUTPUT 1 -p tcp --sport 6004 -j ACCEPT
```

That recovers ~half the +35% on most rulesets.

## Profile-guided optimization (PGO)

For a fixed-workload deployment (you know your read/write mix, command
mix, conn count), PGO lets LLVM optimize the binary using runtime
profile data. Measured 1-10% across workloads on the lx64 reference;
biggest on `drain_inbound` and the dispatch loop.

```sh
# Step 1: build instrumented
RUSTFLAGS="-Cprofile-generate=/tmp/pgo" cargo build --release

# Step 2: collect profile by running a representative workload
LLVM_PROFILE_FILE=/tmp/pgo/kevy-%m_%p.profraw \
  ./target/release/kevy --port 6004 --no-aof &
# In another shell: run your actual production-shaped workload for ~30s
# (redis-benchmark / your real client / etc).
kill %1
sleep 3  # let profile data flush

# Step 3: merge profile data
llvm-profdata=$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata
$llvm_profdata merge -o /tmp/pgo/merged.profdata /tmp/pgo/*.profraw

# Step 4: rebuild with profile-use
cargo clean
RUSTFLAGS="-Cprofile-use=/tmp/pgo/merged.profdata" cargo build --release
```

Requires `rustup component add llvm-tools-preview` for `llvm-profdata`.
The merged.profdata file is ~70 KB for kevy; ship it alongside the
source so any rebuild reuses the same profile until your workload
changes shape.

PGO is NOT shipped in upstream releases because it's workload-specific.
Most prod kevy users won't notice the 1-10%; the deployments that
care should run the recipe above.

## `io_uring` SQPOLL — investigated, not shipped

Kernel polls the io_uring submission queue from a dedicated thread —
removes `io_uring_enter` syscall per op.

The wire-level support exists in `kevy_uring::IoUring::new_sqpoll`,
but it is **not wired into the shard reactor** and we do not recommend
applying it on top of kevy's thread-per-core layout. Each ring spawns
one kernel poll thread, so N shards spawn N additional 100%-spin
kernel threads contending for the same cores as the shard threads. On
the lx64 reference (10 shards on 16 cores) this **regressed
throughput 2–15×** at -c1 and -c50.

SQPOLL belongs to single-threaded reactor designs with a spare core
budget for the poll thread. kevy's per-core design already saturates
the CPU; adding a kernel poll thread halves it. See attack D5 in
`bench/PERF-ATTACK-LOG-2026-06-20.md` for the measurement detail.

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
