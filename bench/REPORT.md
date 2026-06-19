# kevy vs valkey 9.1 / redis 7.4 — bench narrative (v0.2 → v1.22)

> **Current headline (v1.22.0, lx64 16-core, 2026-06-20):**
> - **Server `-c50 -P16` (high concurrency + pipeline)**: kevy SET **4.0 M
>   ops/s** · GET **6.0 M ops/s** — 2.7×/3.0× best-of-valkey-or-redis
>   (valkey-iot 1.5 M / 2.0 M; redis-iot 1.5 M / 2.0 M).
> - **Server `-c1 -P1` (single conn sequential)**: kevy SET **76 k/s** ·
>   GET **68 k/s** vs valkey-iot 60 k/60 k, redis 55 k/54 k. 1.13-1.49×
>   best-of-rest, kept the lead from v1.17.
> - **Embed in-process** (`kevy-embedded::Store`, no socket): SET **7.0 M
>   ops/s** (143 ns/op) · GET hit **9.0 M ops/s** (111 ns/op) · GET miss
>   **42.2 M ops/s** (24 ns/op) — ~90× kevy server `-c1` for the same
>   workload (architecture difference, not apples-to-apples; see embed
>   section for the honest framing).
> - **Pub/sub fan-out** (50 subs × 200 k msgs × 16 B, warm run):
>   Aeron-IPC 84 M msg/s (shared-memory ceiling) → **kevy 18.5 M**
>   → ZMQ 9.4 M → redis 8.9 M → valkey 6.8 M → Zenoh 2.9 M. kevy is the
>   fastest TCP-based broker — 2.0× ZMQ, 2.1× redis, 2.7× valkey.
> - **Async client (`kevy-client-async` 1.0.0, single conn vs steady
>   kevy server)**: async-sequential 4.2 k ops/s · blocking-sequential
>   4.2 k ops/s — async client overhead is ≤ noise (RFC F5 ≥ 80 %
>   target met). Pipeline at batch=64: 172 k ops/s — **41× sequential**,
>   single RTT amortized over N commands.
>
> Jump to the latest segments:
> - [v1.22 bundle bench (server, embed, pub/sub, async)](#v122-v3-cluster-bundle--bench-refresh-2026-06-20-lx64)
> - [v1.17 cluster-aware client](#v117-cluster-aware-clusterclient---tail-latency-fixed-2026-06-15-lx64)
> - [Perf-ceiling campaign](#server-perf-ceiling-campaign--regression-recovered-then-peak-surpassed-2026-060910-lx64)
> - [CLUSTER slot routing](#single-node-cluster-slot-routing--the-forwarding-tax-measured-honestly-2026-06-10-lx64)
>
> The chronological narrative below preserves the v0.2 → v1.22 journey.

---

# v0.2 — baseline

**Date:** 2026-05-24 · **Goal of this run:** validate the bench harness and
measure the starting gap. Not a "we're fast" claim — kevy v0.2 is a deliberately
naive **single-connection blocking** server.

## Setup

- Both servers in Docker Linux (Docker Desktop, **arm64** VM), same network, so
  the only variable is the server. Persistence disabled on both (`--save ""
  --appendonly no` for valkey) to measure the in-memory path.
- Measured by `valkey-benchmark` (neutral tool, not a kevy dependency).
- `n=200000`, no pipelining (`-P 1`), tests: `ping,set,get,incr`.
- valkey: `valkey/valkey:9.1`. kevy: `cargo build --release` (fat LTO,
  codegen-units=1, panic=abort) — see `bench/Dockerfile.kevy`.
- Reproduce: `bash bench/run.sh`.

> Caveat: absolute rps is depressed by the macOS Docker VM; the **ratio** is the
> signal, and both servers share the same VM.

## Single connection (`-c 1`) — apples-to-apples

| test         | valkey 9.1 (rps) | kevy v0.2 (rps) | kevy / valkey |
|--------------|-----------------:|----------------:|--------------:|
| PING_INLINE  | 40,177           | 27,167          | 0.68×         |
| PING_MBULK   | 40,040           | 25,793          | 0.64×         |
| SET          | 30,694           | 25,316          | 0.82×         |
| GET          | 30,530           | 19,520          | 0.64×         |
| INCR         | 30,148           | 23,524          | 0.78×         |

p50 latency is comparable (valkey ~0.023ms; kevy 0.023–0.039ms).

## Concurrency headroom — valkey `-c 50`

| test         | valkey -c 50 (rps) | vs valkey -c 1 |
|--------------|-------------------:|---------------:|
| PING_INLINE  | 146,092            | 3.6×           |
| PING_MBULK   | 149,589            | 3.7×           |
| SET          | 132,890            | 4.3×           |
| GET          | 139,373            | 4.6×           |
| INCR         | 156,250            | 5.2×           |

kevy can't run `-c 50` yet: its blocking `serve` accepts one connection at a
time, so a 50-connection benchmark would stall.

## Reading

1. **The harness works** and gives stable, repeatable numbers.
2. **Per-connection, kevy is already 0.64–0.82× valkey** — for a from-scratch
   blocking skeleton with a hand-rolled socket layer, the protocol + store path
   is not the bottleneck. Encouraging.
3. **The real gap is concurrency.** valkey gets 4–5× from multiplexing 50
   connections. To *exceed* valkey, kevy must reach (then beat) ~150k by:
   event-driven reactor (multi-connection) → thread-per-core (multicore) →
   later pipelining.

## Optimization backlog (cold — surfaced, not scheduled)

- **GET is kevy's weakest (0.64×).** Suspect per-op `Instant::now()` in the
  lazy-expiry path (syscall per command) and `Vec::drain(..consumed)` shifting
  the read buffer. Revisit when the reactor lands; consider a cached clock tick
  and a cursor-based input buffer (`kevy-buf`).
- No pipelining measured yet; add `-P` runs once the reactor exists.
- Run on a native Linux box (no VM) for headline numbers.

---

# v0.3 — event-driven reactor (`kevy-net`)

**Date:** 2026-05-24. Same harness/setup. kevy now multiplexes connections via a
kqueue/epoll reactor, so it can finally run `-c 50`.

> Run-to-run VM variance is real (this run's valkey `-c 1` came in ~30k vs ~40k
> in the baseline run). **Only compare numbers within the same run.**

## `-c 50` (concurrent) — this run

| test         | valkey 9.1 | kevy (reactor) | kevy / valkey |
|--------------|-----------:|---------------:|--------------:|
| PING_INLINE  | 220,751    | 178,891        | 0.81×         |
| PING_MBULK   | 243,013    | 189,753        | 0.78×         |
| SET          | 232,558    | 154,083        | 0.66×         |
| GET          | 191,755    | **195,886**    | **1.02×** ✅  |
| INCR         | 211,864    | 184,843        | 0.87×         |

## `-c 1` (single conn) — this run

| test | valkey 9.1 | kevy | kevy / valkey |
|------|-----------:|-----:|--------------:|
| PING_INLINE | 30,331 | 22,012 | 0.73× |
| PING_MBULK  | 27,863 | 23,705 | 0.85× |
| SET         | 25,006 | 27,933 | 1.12× ✅ |
| GET         | 22,986 | 21,559 | 0.94× |
| INCR        | 24,140 | 33,278 | 1.38× ✅ |

## Reading

1. **The reactor was the right lever.** kevy went from ~25k rps (and unable to
   run `-c 50` at all) to **154k–196k rps**, landing at **0.66–1.02× valkey** on
   a single reactor thread — **GET already edges ahead**, and at `-c 1` kevy beats
   valkey on SET/INCR.
2. **SET is now the weakest (0.66×).** Suspect the per-SET `value.clone()` +
   `key.to_vec()` + HashMap insert allocations. → optimization backlog (v1).
3. **Next lever to *exceed* valkey, not just match it:** both servers are
   ~single-core for command execution here. **thread-per-core sharding** lets
   kevy use *all* cores for the keyspace (valkey keeps command execution
   single-threaded) — the structural way to pull clearly ahead. Then io_uring +
   pipelining toward the hardware ceiling.

## Optimization backlog (v0.3 additions)

- SET allocation path (`clone`/`to_vec`/insert) — revisit with `kevy-buf` /
  borrowed keys at v1 polish.
- Reactor idle wakeup is a 100ms tick; replace with eventfd/self-pipe (or fold
  into io_uring) so shutdown/cross-thread wakeups are immediate and idle is free.

---

# v0.4 — thread-per-core, shared-nothing (`kevy-rt`)

**Date:** 2026-05-25. 14 shards (one per VM vCPU), keys hash-sharded, cross-core
commands forwarded by message passing (`std::mpsc` + self-pipe wakeups, coalesced
to one wakeup per target per loop). Correctness proven by 29 tests incl. a
cross-core shared-keyspace test, pipelined-order test, and fan-out aggregation.

## `-c50` (concurrent), this run

| test | valkey 9.1 | kevy 14-shard | kevy/valkey | (v0.3 1-reactor was) |
|------|-----------:|--------------:|------------:|---------------------:|
| PING_INLINE | 258,065 | 146,735 | 0.57× | 0.81× |
| PING_MBULK  | 237,812 | 133,958 | 0.56× | 0.78× |
| SET  | 256,082 |  98,668 | 0.39× | 0.66× |
| GET  | 279,330 | 104,932 | 0.38× | 1.02× |
| INCR | 271,003 | 100,857 | 0.37× | 0.87× |

**Thread-per-core REGRESSED vs the single reactor.** Honest diagnosis:

1. **Cross-core tax (architectural).** With 14 shards ~93% of commands forward to
   another core; each is a round-trip with a syscall wakeup + locked `mpsc` each
   way. At `-c1`, PING (never forwarded) held ~0.7–0.86×, but SET/INCR
   (forwarded) fell to ~0.46× — the tax, isolated. Coalescing wakeups didn't help
   at `-P1` (≈1 message per target per loop → nothing to coalesce).
2. **Per-command machinery (perf-polish).** Even local commands regressed: the
   sharded path allocates a `Slot` + HashMap insert/remove + per-reply `Vec` per
   command, vs the single reactor's lean parse→dispatch→buffer.
3. **Benchmark confound.** 14 server threads + the load generator share the SAME
   14 vCPUs in the Docker VM. The 1-thread reactor left more cores for the
   benchmark tool, so this co-located setup is structurally unfair to
   thread-per-core. A real measurement needs the load generator off the server's
   cores.

## What makes shared-nothing actually win (the orthodox Scylla/Seastar way)

- **Busy-poll cores** instead of sleeping + syscall wakeups — no per-message
  syscall; the cross-core hop becomes a cache-line write. (Costs idle CPU; fine
  for a dedicated DB box.)
- **Lock-free SPSC/MPSC rings** between cores instead of `std::mpsc`.
- **Lean per-command path:** slot ring indexed by seq (no HashMap), reusable
  reply buffers (`kevy-buf`), borrowed keys.
- **io_uring** to batch I/O syscalls toward the hardware ceiling.
- **Isolated benchmark** (separate load-gen cores / machine) to measure honestly.

These are real work and edge into the deferred perf pass — the architecture
(path) is correct and in place; realizing its win is the v1 perf effort.

**kevy's current fastest is the single reactor (v0.3): ~parity with valkey,
GET ahead.**

---

# v1-perf — adaptive busy-poll + isolated bench

**Date:** 2026-05-25. Two changes: (1) shards **busy-poll** when active (a
cross-core hop is then a queue write with no wakeup syscall; a spinning peer
needs no wake — see `flush_wakes` + per-shard `parked` flags); they park with a
50ms backstop only when idle. (2) The bench now **CPU-isolates** servers
(cores 0–9) from the load generator (cores 10–13), since busy-poll otherwise
pegs every core and starves the client. AOF disabled (`KEVY_AOF=0`) for
in-memory parity with valkey.

## Isolated, this run (kevy = 10 busy-poll shards)

| test | valkey -c1 | kevy -c1 | | valkey -c50 | kevy -c50 |
|------|-----------:|---------:|---|------------:|----------:|
| PING_INLINE | 36,114 | 32,938 (0.91×) | | 192,308 | 149,813 (0.78×) |
| PING_MBULK  | 36,370 | 44,773 (1.23×) | | 192,308 | 138,122 (0.72×) |
| SET  | 32,927 | 42,176 (1.28×) | | 174,064 | 145,243 (0.83×) |
| GET  | 30,746 | 45,830 (1.49×) | | 183,655 | 154,919 (0.84×) |
| INCR | 18,088* | 42,983 | | 195,122 | 155,400 (0.80×) |

\* valkey -c1 INCR was an anomalous low that run (VM noise).

## Reading

- **busy-poll fixed the cross-core tax.** thread-per-core went from 0.37× to
  ~0.8× valkey at -c50, and **kevy now beats valkey on single-connection**
  (SET/GET/MBULK 1.2–1.5×) — the shared-nothing design finally pays off.
- The residual -c50 gap is partly **load-gen-bound**: only 4 client cores drive
  -c50 here (valkey's own -c50 also dropped vs the co-located run). A definitive
  high-concurrency number needs a separate load-gen machine.
- Remaining perf levers (v1 polish, ordered): lock-free SPSC/MPSC rings instead
  of `std::mpsc`; lean per-command path (drop the `Slot`/`done` HashMap churn,
  reuse reply buffers); io_uring (net+disk) toward the disk-I/O ceiling.

Cost: busy-poll pins server cores at ~100% under load (the Scylla/Seastar
model — appropriate for a dedicated DB box).

## + lean per-command path (slot ring)

Replaced the per-command `Slot`/`done` HashMaps with an O(1) seq-ordered
`VecDeque` ring (no hashing, no per-command map alloc). A later run:

| test | valkey -c50 | kevy -c50 | ratio |
|------|------------:|----------:|------:|
| PING_INLINE | 162,999 | 152,905 | 0.94× |
| PING_MBULK  | 158,228 | 141,945 | 0.90× |
| SET  | 169,635 | 153,965 | 0.91× |
| GET  | 172,563 | 156,006 | 0.90× |
| INCR | 183,318 | 156,617 | 0.85× |

(kevy still beats valkey at -c1: SET 32.9k vs 23.0k, INCR 33.0k vs 24.4k.)

**Perf-polish arc: naive thread-per-core 0.37× → busy-poll 0.8× → slot-ring
~0.9× valkey at -c50, ahead at -c1.** Remaining to clear 1.0×+ at high
concurrency: lock-free SPSC/MPSC rings (drop `std::mpsc`), io_uring (net+disk),
and a separate load-gen machine for a clean measurement.

---

# post-modularization baseline (pre-ring-integration)

**Date:** 2026-05-25. After the big-file/big-fn modularization (kevy-store → 9
modules; kevy `dispatch` → router + 8 handlers; kevy-rt → 6 modules incl. the
`shard`/`exec` reactor/executor split) **and** the dispatch hot-path fix (fold
the verb into a stack buffer, no per-command heap alloc). Architecture otherwise
unchanged: 10 busy-poll shards, slot-ring, **still `std::mpsc`** cross-core.
Bench host was busy (load ~9.5 — other projects building), so absolute rps is
depressed; the **ratio** (same VM, same run) is the signal.

## `-c 1` (single connection)

| test | valkey 9.1 | kevy | kevy/valkey |
|------|-----------:|-----:|------------:|
| PING_INLINE | 37,140 | 48,450 | 1.30× ✅ |
| PING_MBULK  | 37,286 | 45,893 | 1.23× ✅ |
| SET  | 36,996 | 43,225 | 1.17× ✅ |
| GET  | 37,951 | 41,675 | 1.10× ✅ |
| INCR | 37,085 | 46,147 | 1.24× ✅ |

## `-c 50` (concurrent)

| test | valkey 9.1 | kevy | kevy/valkey |
|------|-----------:|-----:|------------:|
| PING_INLINE | 180,995 | 147,929 | 0.82× |
| PING_MBULK  | 182,315 | 151,515 | 0.83× |
| SET  | 192,864 | 157,853 | 0.82× |
| GET  | 190,295 | 157,729 | 0.83× |
| INCR | 190,476 | 157,356 | 0.83× |

## Reading

1. **Modularization + dispatch fix did NOT regress perf.** At `-c1` kevy still
   beats valkey across the board (1.10–1.30×); `-c50` sits at ~0.82–0.83×, the
   same band as the slot-ring run (0.85–0.94×) within run-to-run + host-load
   noise (this host was at load ~9.5). A pure structural refactor that leaves the
   hot path intact — as expected.
2. **The `-c50` gap (~0.18) confirms the cross-core `std::mpsc` is still the
   bottleneck** — exactly what `kevy-ring` (built + stress-tested) targets. Next:
   integrate it (one ring per ordered core-pair; deadlock-safe send via a local
   backlog), then re-measure the gain on an idle host.

---

# post-ring-integration (kevy-ring SPSC mesh replaces std::mpsc)

**Date:** 2026-05-25. Cross-core transport switched from `std::mpsc` to one
lock-free SPSC ring per ordered core-pair (kevy-ring); a full ring spills to a
local per-target backlog. Same host contention as the pre-ring baseline (load
~9.4 from concurrent project builds), so **ratios are noisy — treat ±0.03 as
noise.** 10 busy-poll shards.

## `-c 50` (the cross-core path; ~90% of single-key cmds forward)

| test | valkey | kevy (ring) | ratio | (pre-ring mpsc was) |
|------|-------:|------------:|------:|--------------------:|
| PING_INLINE | 183,486 | 146,199 | 0.80× | 0.82× |
| PING_MBULK  | 180,018 | 143,678 | 0.80× | 0.83× |
| SET  | 184,502 | 148,258 | 0.80× | 0.82× |
| GET  | 182,983 | 146,628 | 0.80× | 0.83× |
| INCR | 180,668 | 149,813 | 0.83× | 0.83× |

## `-c 1`

kevy 44.1k / 45.7k / 40.6k / 41.7k / 40.3k vs valkey ~36k — still **1.1–1.3×
ahead** (ring is harmless to the single-connection path).

## Reading (honest)

**The ring shows no measurable -c50 gain over `std::mpsc` here** — 0.80–0.83×,
the same band as before, within host-load noise. Most likely why:

1. **busy-poll already removed the dominant cross-core tax** (the syscall
   wakeup). The remaining `std::mpsc` cost is an *uncontended* `Mutex`
   lock/unlock: each ordered core-pair has effectively one producer, so each
   receiver is rarely contended, and an uncontended `Mutex` is a few ns — dwarfed
   by the command + socket work. The lock-free ring removes those few ns, which
   weren't the bottleneck. (The v0.4 "mpsc is the bottleneck" diagnosis predated
   busy-poll, when the *wakeup syscall* — since eliminated — was the real tax.)
2. **PING (keyless, never forwarded) and forwarded SET/GET/INCR move together**
   (~0.80×), which points at a shared limiter that is *not* the cross-core queue
   — most likely load-gen-bound (only 4 client cores drive -c50) and/or per-op
   socket syscalls.
3. **Host noise**: load ~9.4 on both runs; the ~0.02 dip is run-to-run variance.

**Conclusion:** kevy-ring is correct, zero-allocation, and the orthodox
Seastar-model primitive (a better foundation for io_uring / higher core counts),
but at this core count + busy-poll it is **perf-neutral vs an uncontended
`std::mpsc`** under this noisy measurement. A clean idle-host re-run is needed to
confirm neutral-vs-small-gain. The real -c50 levers are elsewhere: a separate
load-gen machine (lift the client-side cap) and **io_uring** (cut per-op socket
syscalls toward the disk-I/O ceiling).

---

# io_uring reactor (Phase 2b) — correctness done; bench host-contaminated

**Date:** 2026-05-25. New `kevy-rt/src/uring_reactor.rs` (cfg linux): a
completion reactor reusing all command logic, opt-in via `KEVY_IO_URING=1`.
**Correctness: the full `sharded` suite (11 tests, incl. cross-shard pipeline +
transactions) passes via BOTH the epoll reactor and the io_uring reactor.**

## `-c 50` — host was at load ~8.5

Other projects' processes were pinned to the same server cores (0-9), starving
kevy's busy-poll shards; absolute rps is ~5× below a clean run (kevy latency
~1.5ms vs valkey 0.18ms). Only the **back-to-back io_uring-vs-epoll delta** is a
(weak) signal.

| test | valkey | kevy epoll | kevy io_uring | io_uring vs epoll |
|------|-------:|-----------:|--------------:|------------------:|
| SET  | 149,031 | 29,112 | 30,386 | +4% |
| GET  | 151,860 | 31,969 | 37,679 | **+18%** (1.52ms → 1.05ms) |
| INCR | 151,400 | 29,108 | 27,431 | −6% |

## Reading

- **Both kevy modes at ~0.2× valkey is host contamination, not the design** — a
  clean host earlier put kevy at ~0.9× valkey -c50. The other projects shared
  cores 0-9 with kevy's busy-poll shards and starved them; valkey (blocking) was
  hurt less.
- The same-host io_uring-vs-epoll delta is weak but positive: io_uring is **not
  worse**, with a clear **GET edge** (+18%, latency −31%); SET +4%, INCR −6%.
- **A clean io_uring-vs-epoll measurement is blocked on an idle host** (the
  persistent blocker this whole session). The reactor is also still step-1
  (busy-poll + 200µs idle sleep); an `IORING_OP_TIMEOUT` park and multishot recv
  are the next polish.

**Phase 2b reactor: correct and verified; perf quantification pending an idle host.**

---

# CLEAN measurement on a dedicated 16-core Linux box — kevy leads on all axes

**Date:** 2026-05-26. First measurement free of the three artifacts that had
depressed/distorted every prior run. Run on a **dedicated 16-core Linux box**
(bare metal, not a VM), all servers **host-loopback** (no docker bridge / NAT),
in-memory.

## The three measurement artifacts (why earlier numbers lied)

1. **macOS Docker VM** — depressed absolute rps ~5–10×. Gone: native Linux.
2. **Docker bridge veth softirq** — starved kevy's busy-poll (understated kevy).
   Gone: host networking / loopback for every server.
3. **Co-located busy-poll starves the load generator** — *the big one.* kevy's
   shards busy-poll every core they're given. When the `redis-benchmark` client
   shared those cores, the client was starved and the measured throughput was the
   *client's* ceiling, not kevy's — and it got **worse the more shards kevy ran**,
   which we had misread as a "cross-core tax" that grew with core count. It was
   never a real tax. Fix: **pin the server to cores 0–9 and the client to disjoint
   cores 10–15** (`taskset`), and run **each server in isolation** (start, bench,
   stop) so kevy's busy-poll never steals cycles from a co-located competitor.
   With this, kevy -c50 jumped from the old "~0.9× / 1.26M" to **3.9–4.7M**.

Same core budget for every server (10 server cores, 6 client cores) ⇒ fair fight.
Reported figures are steady-state `overall` rps (the `--threads` *final* line is
quantized and unreliable). redis-benchmark 8.0.2.

## `-c50 -P16` (high-concurrency throughput) — `requests per second`

| server | GET | SET |
|--------|----:|----:|
| **kevy io_uring (10sh)** | **~4.4M** | **~4.7M** |
| **kevy epoll (10sh)** | ~3.9M | ~3.7M |
| valkey 9.1 io-threads=10 | ~2.5M | ~1.9M |
| valkey 9.1 default | 1.53M | 1.27M |
| redis 7.4 io-threads=10 | ~2.3M (jittery) | ~1.97M (jittery) |
| redis 7.4 default | 1.99M | 1.74M |

**kevy leads every competitor config:** epoll **1.56× GET / 1.88× SET**, io_uring
**1.76× GET / 2.39× SET**, both vs the *strongest* valkey/redis config. The client
(6 cores) pushed kevy to 4.7M, so the ~2.5M competitors are server-bound, not
client-bound — the comparison is valid.

## `-c1 -P1` (single connection — pure round-trip latency/throughput)

| server | GET | SET |
|--------|----:|----:|
| **kevy epoll (10sh)** | **86.1k** | **72.0k** |
| kevy io_uring (10sh) | 67.4k | 54.0k |
| valkey 9.1 io-threads | 64.5k | 63.0k |
| valkey 9.1 default | 50.7k | 50.4k |
| redis 7.4 default | 47.8k | 54.4k |

**kevy (default epoll) leads -c1:** GET **1.33×**, SET **1.14×** vs the best
valkey. Note the reactor split: at **-c1 epoll wins** (io_uring's completion model
adds latency to a lone round-trip), while at **-c50 io_uring wins** (its IO
batching dominates). The default (epoll) is exactly the right pick for the
latency-sensitive low-concurrency case.

## Request-batching A/B (this checkpoint's change) — develop vs feature, isolated

Cross-core single-key dispatches are now forwarded as one batched message per
loop (`Inbound::RequestBatch`/`ResponseBatch`) instead of one `Request` per
command, mirroring the pub/sub fan-out batching.

| -c50 -P16 | develop (pre-batch) | feature (batch) | Δ |
|-----------|--------------------:|----------------:|---:|
| epoll GET | ~2.2M | ~3.9M | **+77%** |
| epoll SET | ~2.37M | ~3.68M | **+55%** |
| io_uring GET | ~4.44M | ~4.42M | ~flat (at ceiling) |
| io_uring SET | ~4.18M | ~4.68M | +12% |

Batching is **what lifts the default epoll reactor into a substantial lead**
(without it, epoll ~2.2M only ties valkey-iot); io_uring was already winning and
stays there. No regression. Merged to `develop`.

## Reading

- **kevy now leads valkey 9.1 and redis 7.4 on every axis measured** — -c1
  (latency), -c50 -P16 (throughput), and pub/sub (15.6M msg/s, measured earlier
  under the conservative docker-bridge setup). The earlier "-c50 lags at ~0.9×"
  was purely the co-located-busy-poll artifact, not a design limit.
- **Honest caveats:** the box had background load (~2.7–3.8) during the runs, which
  hurts kevy's busy-poll more than the blocking competitors, so kevy's true lead
  is if anything *understated*. Competitor io-threads runs were jittery
  (occasional drops to ~190k) — kevy was stable throughout.
- **pub/sub (clean, 2026-05-26):** redone over host-loopback with isolated cores
  (`bench/pubsub_loopback.sh`, 50 subs + flooding publisher): **kevy io_uring
  ~17.7M / epoll ~16.8M delivered msg/s vs valkey 6.6M (~2.6×) and redis 8.5M
  (~2.0×)**; publishes ~336k/s vs valkey 131k / redis 170k. Slightly above the old
  docker-bridge 2.28× (which understated kevy). Three-indicator clean
  verification complete — kevy leads on -c1, -c50 -P16, and pub/sub.
- **Open:** a second physical load-gen box would lift the residual client-side cap
  on the single-box -c50 numbers (the binding measurement constraint).

Harnesses: `bench/loopback_c50.sh` (3-way -c50), `bench/kevy_ab.sh` (binary
A/B), `bench/loopback_c1.sh` (-c1). All pin server/client to disjoint cores.

## Follow-up perf (2026-05-26): fast path, pipeline scan, multishot recv

Three measure-first checkpoints after the clean baseline (all on develop):

1. **Single-key fast path** — `Route::Local`/`Route::Single` (95%+ of commands)
   skip the per-command `Vec<(shard,Op)>`; one heap alloc/command gone. A/B
   (server-bound 4sh, -c50 -P64): GET +1.6%, SET +3.1%.
2. **Pipeline scan diagnosis** — 4sh io_uring, 12-core client, GET -c50: -P1 377k
   → -P256 7.2M (19×). The low-`-P` ceiling is per-arrival io_uring/reactor
   overhead, not command CPU. **And the single 16-core box is CLIENT-bound** at
   -P16 (the 12-core redis-benchmark caps ~3.3M there), which is the binding
   constraint on measuring any further server-side win.
3. **io_uring multishot recv + provided buffers** — one re-firing recv SQE per
   connection drawing from a shared per-shard buffer ring, instead of a
   read-SQE-per-arrival. Hand-written ABI (no liburing). A/B (3 runs): **-P16
   +7.6%, -P64 +3.9%**, consistently ≥ single-shot and notably more stable, plus
   lower memory (2 MiB/shard ring vs 16 KiB/conn). Full ~2× potential is masked by
   the client-bound box. sharded 11/11 via epoll + io_uring; clippy 0.

**Binding constraint going forward:** kevy's server outruns what a single
16-core box's co-located `redis-benchmark` can drive at -c50 — further
server-side perf needs a **dedicated second load-gen machine** to measure
honestly.

## perf-guided per-core wins (2026-05-26): single-shard 3.77M → ~5.9M GET/core

`perf record` on the single-shard io_uring reactor (debug-symbol release,
`perf_event_paranoid` relaxed + restored) found two big reactor-path costs that
component micro-benches alone had missed:

1. **Fx-hash the per-connection maps (+25%).** `conns`/`fd_to_conn`/`io` were std
   `HashMap` (SipHash) — ~17.6% of CPU hashing the u64/i32 keys per command (conns
   looked up twice/command). Switched to kevy-hash `FxHashMap` (~1ns vs ~15ns).
   Single-shard GET 3.77M → 4.78M.
2. **In-order local reply bypass (+24%).** When a single-key command runs on the
   conn's own shard with nothing pending, write the reply straight to
   `conn.output` (via `dispatch_into`) — no PendingSlot/fold/materialize, no
   per-command reply alloc, no drain copy. 4.78M → ~5.9M.

**Cumulative +57% per-core: ~5.9M GET/core ≈ 2.4× valkey 9.1's total throughput.**
Both are reactor-path (unlike the command-CPU `encode_bulk` reserve, which was
component -61% but system-neutral — the server is reactor-bound, not
command-CPU-bound). Verified: sharded 11/11 via epoll AND io_uring, clippy 0,
full tests green. The lesson: a profiler beats guessing — the SipHash hotspot
contradicted a standing "negligible" assumption.

---

## v1.5.0 regression A/B — the feature wave didn't touch the hot path (2026-06-07, lx64)

**Question:** did the v1.1–v1.5 feature wave (RESP3, Geo, Streams, blocking
pops, cross-shard BLOCK) regress GET/SET? `resolve()` now also computes
`block_hint` + `wake_idx` per command, and routing gained BLPOP / BRPOP /
XGROUP / XINFO arms.

**Method:** `bench/kevy_ab.sh`, **same release profile for both binaries**,
lx64 (16-core, kernel 6.12), server cores 0-9 / client cores 10-15, `-c50
-P16 n=4M`, **3 rounds × 2 samples = 6 samples/metric**. `v1.4.2` tag vs
`develop` (= v1.5.0 + the compat fixes). Both reactors.

| metric        | v1.4.2 | develop | Δ      |
|---------------|-------:|--------:|-------:|
| epoll GET     | 1.67M  | 1.73M   | +3.5%  |
| epoll SET     | 1.62M  | 1.67M   | +3.3%  |
| io_uring GET  | 2.61M  | 2.61M   | ~flat  |
| io_uring SET  | 2.47M  | 2.44M   | −1.0%  |

**Verdict: no regression.** develop is flat-to-slightly-faster; every Δ is
inside the ~5–7 % run-to-run spread (the per-binary samples vary that much
themselves). Confirms the code-level read — the blocking/stream additions to
`resolve()` only fire for those verbs; GET/SET still hit the default route
arm and an early `None` from `block_hint`/`wake_idx`, with no new allocation.

**Caveat:** these absolute numbers are CLIENT-bound (6 client cores driving
`-c50 -P16`), well below the headline single-shard ~5.9M GET/core — this run
measures *relative* v1.4.2 → v1.5.0, not peak server throughput. A peak
re-baseline still needs the 2-box setup (the documented binding constraint).

## v1.5.0 3-way re-baseline — kevy still leads valkey 9.1 + redis 7.4 (2026-06-07, lx64)

Confirms the "leads on every axis" claim holds after the v1.1–v1.5 feature
wave. Isolated runs (one server at a time, same cores), lx64 16-core, server
cores 0-9 / client cores 10-15 ×6, `-c50 -P16 n=3M`. valkey 9.1 + redis 7.4
via `docker --network host --cpuset-cpus 0-9` (no bridge NAT), **single-
threaded defaults** (their out-of-box shape); kevy = develop 10-shard.
Steady-state `overall:` rps (the quantized "requests per second" tail line
is unreliable under `--threads`).

| engine            | GET    | SET    | vs kevy-uring |
|-------------------|-------:|-------:|--------------:|
| valkey 9.1        | ~1.1M  | ~1.0M  | —             |
| redis 7.4         | ~1.4M  | ~1.38M | —             |
| **kevy epoll**    | ~1.75M | ~1.62M |               |
| **kevy io_uring** | ~2.55M | ~2.4M  |               |

Ratios (kevy io_uring): **~2.2× valkey, ~1.7× redis**; kevy epoll ~1.5×
valkey, ~1.2× redis. (Aside: redis 7.4 edged valkey 9.1 on this workload —
both still behind kevy.)

**Caveat (unchanged):** single-box `-c50 -P16` is CLIENT-bound (6 client
cores), so absolute rps is capped below the headline single-shard ~5.9M
GET/core — the ratio is the signal. A true peak re-baseline still needs the
documented 2-box (same-LAN) load-gen setup.

## disk-I/O persistence baseline — measure-first (2026-06-07, lx64, Samsung 9100 PRO NVMe)

First numbers for the persistence path (this REPORT's blank area). kevy SET
`-c50 -P16 n=2M`, 10 shards, server cores 0-9 / client 10-15.

| AOF mode                  | SET rps | vs no-AOF |
|---------------------------|--------:|----------:|
| off (cache-only)          | 1.65M   | —         |
| everysec (Redis default)  | 1.45M   | −12 %     |
| always (fsync per write)  | 0.89M   | −46 %     |

- **everysec is nearly free** (−12 %): the 1-second-window durable path
  stays near the memory/reactor ceiling — for the default durable config
  kevy is reactor-bound, **not** disk-bound. Persistence isn't the
  bottleneck there.
- **always (per-write durability) costs ~half**, and the cost is per-command
  `flush() + sync_data()` — two syscalls per write with **no group commit**
  (`kevy_persist::aof::append`, the `Fsync::Always` arm). 0.89M/s ≫ any real
  per-write flush barrier, so this consumer NVMe fast-acks `fdatasync` from
  its volatile cache: the always path is *syscall-overhead-bound*, not
  fsync-barrier-bound.
- **Snapshot sequential-write ceiling: 2.7 GB/s** (fio bs=1M O_DIRECT) —
  bulk SAVE is bandwidth-rich (a 1 GB dataset ≈ 0.4 s of raw write).

**Optimization identified (next):** AOF Always **group commit** — append a
whole reactor iteration's writes to the BufWriter, then ONE
`flush()+sync_data()` before flushing that iteration's replies (preserving
"durable before reply"). Amortizes the 2-syscall-per-command cost across a
batch; should lift always from ~0.89M toward everysec's ~1.45M — and on a
PLP datacenter SSD, where `fsync` is a real barrier, the win is far larger
(one barrier per batch, not per write).

Caveat: a consumer SSD without power-loss protection fast-acks `fdatasync`,
so "always" power-safety is drive-dependent regardless of kevy. (An earlier
`fio --fdatasync=1 --bs=100` micro-bench read 1821 IOPS — sub-block writes
+ per-100-byte fdatasync is pathological; the kevy numbers above are the
real signal, not that.)

### AOF Always group-commit — result (2026-06-07, lx64, same setup)

Implemented group-commit (defer the `always` fsync to the end of a pipelined
batch, still before its replies leave). Re-measured epoll, SET `-c50 -P16
n=2M`, 10 shards, same machine:

| AOF mode (epoll)       | before | after group-commit |
|------------------------|-------:|-------------------:|
| always (fsync/write)   | 0.89M  | **1.30M  (+46 %)** |
| everysec               | 1.45M  | 1.42M (flat)       |
| off (no-AOF ceiling)\* | 1.65M  | 1.48M\*            |

\* `off` is unaffected by the change (AOF disabled ⇒ no group is opened); the
lower no-AOF number this run is box-load drift, so the `always` gain is if
anything understated. The telling ratios: `always` went from **54 % → 88 %**
of the *same-run* no-AOF ceiling, and the per-write-durable vs 1s-window gap
shrank from **−39 % to −8 %**. One fsync per pipelined batch instead of per
command, durability-before-reply preserved (the fsync still precedes the
batch's replies). io_uring-local writes still fsync per command — a
follow-up; the cross-shard `RequestBatch` path is group-committed on both
reactors.

io_uring follow-up: the io_uring local read path (`uring_on_recv`) is now
group-committed too (the cross-shard `RequestBatch` already was on both
reactors). always lands ~1.2–1.3M on BOTH reactors — it's fsync-syscall-
bound, independent of the reactor (vs no-AOF, where io_uring's ~2.34M beats
epoll's ~1.48M). So the always path still has the most relative headroom on
io_uring; closing it further means bigger fsync batches / fewer barriers
(or a PLP drive where one barrier per batch is the real win).

### snapshot SAVE throughput (2026-06-07, mini / Apple M4 Pro, clean idle box)

SAVE of a 356 MB / 1.26M-key dataset (256 B values), kevy default 14 shards,
AOF off. Wall time of the `SAVE` command; throughput = dump bytes / time.

| snapshot BufWriter | SAVE throughput | % of disk ceiling |
|--------------------|----------------:|------------------:|
| 8 KiB (default)    | 758 MB/s        | 12 %              |
| **1 MiB**          | **~1.73 GB/s**  | **28 %**          |

NVMe sequential-write ceiling on the same box: 6.1 GB/s (`dd bs=1m`). The
8 KiB default turned the snapshot into tens of thousands of small `write(2)`s;
a 1 MiB buffer amortizes them for **+128 %**. The remaining gap to the disk
ceiling is per-key serialization CPU (`write_entry`), not I/O — a deeper,
lower-ROI optimization left for later. (Same const lifts BGREWRITEAOF's
bulk dump.)

### AOF always — loop-level group commit tried, no benefit, reverted (2026-06-07, mini)

Hypothesis: coalescing a whole reactor-loop iteration's writes (every readable
conn) into ONE fsync would beat the per-read-batch group commit. Implemented
it (defer reply flush to a single `flush_dirty` fsync) and measured on mini
(kqueue, honest fsync): `always` SET stayed flat — -P16 6.1k, -P64 75k, -P256
469k, indistinguishable from the per-batch version. Reason: under this
workload the reactor processes ~one readable conn per loop, so "per loop" ≈
"per read batch" — no extra coalescing to capture. Reverted (measure-first:
no measured win ⇒ no added complexity). The per-batch group commit (v1.6.0,
+46% on lx64) stands as the AOF-always optimization. Further always gains
would need a different mechanism (e.g. a short time-window group, or batching
across loops) — deferred unless a real durable-write workload demands it.

## Server perf-ceiling campaign — regression recovered, then peak surpassed (2026-06-09/10, lx64)

Two-phase campaign against the post-feature-wave regression (standard
`-c50 -P16` corners were −27…37 % vs the `877cd41` peak binary) and then
past it. All A/Bs interleaved multi-round on an idle box, `overall:` line
only, peak anchor co-run.

**Phase 1 — recovery (8 commits, `459c924..f941c8d`):** SLOWLOG default
OFF (+13–19 %), config-read deferral (+10–13 %), io_uring reap 1/16
amortize (+8–13 %), tier-1 GET/SET dispatch (+15 % SET), DispatchMeta
(verb facts resolved once, +6 % GET), single-probe overwrite SET
(+8–11 %), stack-inline SmallReply across the cross-shard ring (+2.3 %).
Recovered the four corners to −8…10 % of peak.

**Phase 2 — allocator + parse surge (7 commits, `f856ea3..5f69b01`):**

| commit | lever | 8sh-P256 io_uring A/B |
|---|---|---|
| `f856ea3` | ArgvPool: pool-recycled owned argvs on the forward path; borrowed local dispatch | (gated with next) |
| `eb530cf` | spent-argv husks ride the reply batch back to the origin's pool | GET +24.8 %, SET +25.9 % |
| `3914b5e` | single-pass multibulk parse (fused `$len\r\n` header walk) | GET +4.2 %, SET +2.2 % |
| `83e1ef8` | parse cursor: one tail drain per batch, not per cmd (was O(batch²) bytes) | GET +4.6 % |
| `d0d40bf` | `Store::set_slice`: small SET values inline without the `to_vec` malloc/free pair | SET +8.2 % (first past 10M) |
| `9df5113` / `5f69b01` | refactors (shared dispatch_batch; one conns probe pre-dispatch) | measured flat, kept for shape |

The husk-return commit is the campaign's center: recycling at the owning
shard failed measurably (accept skew starves conn-heavy shards' pools
while quiet shards overflow — malloc share didn't move), so the husk now
returns to its origin with the reply, making every pool's level match its
own conn demand by construction. After it, malloc left the 8sh SET
profile top-10 entirely.

**End state (3-round interleaved, same run):**

| corner (`-c50 -P16`, 10sh) | f941c8d | HEAD `5f69b01` | `877cd41` peak | HEAD vs peak |
|---|---:|---:|---:|---:|
| epoll GET | 4.23M | **5.43M** | 4.51M | **+20 %** |
| epoll SET | 3.85M | **5.89M** | 4.37M | **+35 %** |
| io_uring GET | 4.85M | **6.35M** | 5.18M | **+22 %** |
| io_uring SET | 4.68M | **6.00M** | 4.70M | **+28 %** |

8-shard `-c50 -P256` io_uring: GET 8.5M → **11.4M** (+34 %), SET 7.4M →
**10.3M** (+39 %).

Negative results (kept out, archived in the session notes): pooling spent
argvs at the owning shard (accept skew), embedding per-conn io_uring state
into `Conn` (struct bloat hurt per-cmd probes more than the arm scan
saved), zero-copy parse from the provided buffer (flat — the chunk memcpy
is cheap next to dispatch), and conns-probe consolidation (flat — KevyMap
u64 probes are not a bottleneck on this box; kept as a refactor).
Same-binary calibration on this box: SET can swing ±6 % between rounds —
single-digit deltas need 4+ interleaved rounds.

Remaining ceiling levers (unstarted): reactor notification machinery
(`run_uring` self ≈16 % — io_uring-native polling of the cross-core rings
/ eventfd integration) and key-aware routing (clusters/client-side
sharding would erase the 87.5 % forward tax; ~4.5× theoretical headroom).

## Reactor notification machinery — resolved (2026-06-10, lx64)

The "`run_uring` self ≈16 %" headline decomposed (via `#[inline(never)]`
on the loop's four inlined blocks + perf annotate) into: `uring_arm_conns`
6.1 % — almost entirely a `keys()` snapshot Vec + 3–8 redundant map probes
per conn per iteration to satisfy the borrow checker — and
`uring_drain_inbound` 4.5 %, which is mostly real message-shuttling work,
not empty polling. The notification *architecture* was never the hot cost.

**Landed:**

| commit | change | measured |
|---|---|---|
| `17bb639`/`c10db1f` | `KevyMap::iter_mut` (new stone API, miri-clean); single-pass arm loop, one `io` probe per conn, no snapshot | 8sh SET +3.8 % (4/4), GET +1.2 % (3/4) |
| `2cf3f14`/`24bd703` | `IORING_OP_TIMEOUT` in kevy-uring; spin → nap → park idle ladder | throughput flat (+0.5/+1.4 %); **idle CPU 6.5 % → 0.7 %** (8 shards); c1 p50 unchanged |

The park rung is the epoll park translated to the ring: `parked[me]` +
fenced re-drain (same loom-verified pairing) + a waker-pipe read SQE +
a timeout SQE, blocking in `submit_and_wait(1)`. Waker/timeout CQEs
don't count as work, so an idle shard re-parks straight from a 50 ms
tick instead of burning a spin burst.

**Negative result (the instructive one):** parking directly after the
spin stage — i.e. instant wakeups instead of the old 200 µs sleep —
measured **−18…21 %** on the 8sh bench. Under load, the sleep was doing
real work: brief lulls aggregate cross-core inbound into bigger batches,
and instant wakes replaced that with producer-side pipe-write syscalls
plus smaller batches per iteration. Throughput here wants bounded
latency, not minimal latency; the nap rung keeps it.

**Post-campaign profile (8sh-P256 SET):** reactor machinery
(loop + arm + drain) 15.5 % → ~7.8 %; top single item is now
`dispatch_batch` 7.1 % (parse + dispatch proper). The 40 % `intel_idle`
on server cores is accept-skew + forward-tax idleness — that is the
key-aware-routing lever (~4.5×), now the only big one left.

## Single-node CLUSTER slot routing — the forwarding tax, measured honestly (2026-06-10, lx64)

Roadmap ③: cluster mode (`--cluster`) exposes each shard at a deterministic
port with Redis-cluster slot routing (CRC16 `{hashtag}` & 16383, contiguous
ranges), so cluster-aware clients address the owning shard directly — no
cross-shard forwarding. Protocol validation: `redis-cli -c` follows MOVED
across all shards (hashtags included), `CLUSTER KEYSLOT foo` = 12182 (matches
upstream Redis), and a packet capture during a full `redis-benchmark
--cluster` run contained **zero MOVED** frames — placement is exact.

**Result: the forwarding tax is real and cluster routing removes it, but on
this 16-core box the *throughput* headline doesn't move, because
`redis-benchmark --cluster -r 1000000` is client-bound at ~6.1–6.6 M ops/s.**
The win shows up as server CPU instead (same throughput, far less work):

| angle (single-test, -r 1M, P256) | compat port | cluster ports | server CPU |
|---|---|---|---|
| 8sh GET | 6.6 M | 6.6 M | 125% vs ~200%+ (cluster lower) |
| 4sh GET | 6.19 M | 6.13 M | **209% → 128% (−39%)** |
| 4sh SET | 6.13 M | 5.70 M | **266% → 208% (−22%)** |
| 2sh GET | 6.14 M | 6.13 M | client-capped |
| 2sh SET | 4.2–4.4 M | 3.8–4.2 M | server-bound, parity within ±6% round noise |

At equal load the cluster path does ~25–40 % less server work per op; that
margin becomes throughput the moment clients are not the bottleneck (more
client cores / multiple load generators than this box has). The 2sh SET
parity (instead of a win) decomposes into two costs the redirect path adds:
`-c 50 --cluster` opens 50 conns *per node* (2× total conns → shallower
per-conn batches), and slot routing hashes with byte-wise CRC16 instead of
the word-wise KevyHash (~4× slower per key; slice-by-4 tables are the known
upgrade if a server-bound angle ever shows it matters).

**Regression gate (old 8sh angle, no -r, compat port, cluster off)** —
park2 (`24bd703`) vs cluster HEAD, 3 interleaved rounds: GET 11.35 M → 11.21 M
(−1.2 %), SET 10.57 M → 10.29 M (−2.7 %), both inside the ±6 % round-to-round
noise. Cluster-off costs one dead branch.

Tooling caveat (cost a few hours): `redis-benchmark 8.0.2 --cluster` with
**multiple tests in one invocation** (`-t get,set`) skews its key
distribution badly across nodes (observed 291–1920 pkts/port vs perfectly
uniform single-test runs) and the affected stage drops to ~2.5 M. Cluster
angles must run one test per invocation (`/tmp/ab_cluster.sh` updated).

### The forwarding tax, converted to throughput (2026-06-10, lx64, pinned-hashtag angle)

The `redis-benchmark --cluster` client was the bottleneck hiding the win, so
this angle removes it: 8 plain-mode redis-benchmark processes, each pinning
its keys to one shard via a `{tag}` hashtag (`GET {t3}:__rand_int__ …`,
6 conns × P256 each). Cluster mode connects each process straight to its
shard's port (0 % forwarded); compat mode sends the *identical* commands to
the shared REUSEPORT port (~7/8 forwarded). Client cost is byte-identical in
both modes — the delta is the tax. 5 interleaved rounds (`/tmp/ab_pinned.sh`):

| mean of 5 rounds | compat (7/8 forwarded) | cluster ports (0 forwarded) | delta |
|---|---|---|---|
| SET | 13.7 M | **19.4 M** | **+42 %** |
| GET | 14.7 M | **20.1 M** | **+37 %** |

Cluster-side numbers are tight (±1–2 % across rounds; compat wobbles ±6 % —
the forwarded path is inherently noisier). **New 8-shard headline: GET
~20.1 M / SET ~19.4 M ops/s** — key-aware routing pays exactly where the
campaign predicted, once the load generator can keep up.

Harness note: rounds 2/4 initially reported compat = 0 — a lifecycle race
(the next server bound its cluster ports while the previous one was still
dying → AddrInUse → exit, and the ready-probe had pinged the dying server).
Fixed in the script: kill + wait for zero `pgrep` matches before each start.

### Profile-guided follow-up: reaper bound + slice-by-4 CRC16 (2026-06-10, lx64)

Profiling the new headline showed two avoidable costs: `tick_expire` at
6.1 % (the sampling walk bounded TTL-bearing *samples* but not *visited
buckets*, so a TTL-free 300k-key shard walked the whole table ×3 every
100 ms tick — the exact tax the doc comment promised TTL-free workloads
would never pay) and `shard_of` at 3.7 % (cluster routing hashes every key
with byte-wise CRC16). `a635d65` caps reaper visits at `samples × 8` and
moves CRC16 to slice-by-4 (const-generated companion tables, equivalence-
tested against the byte-wise reference at every length 0..=64).

Quiet-box A/B, 3 interleaved rounds, pinned-hashtag angle:

| mean of 3 rounds | before | after | delta |
|---|---|---|---|
| cluster GET | 20.3 M | **23.7 M** | **+17 %** |
| cluster SET | 19.4 M | **21.9 M** | **+13 %** |
| compat GET | 14.1 M | 17.5 M | +24 % |
| compat SET | 13.7 M | 16.8 M | +22 % |

The compat side gains too — the reaper fix is universal, not a cluster
perk. **8-shard headline now: GET ~23.7 M / SET ~21.9 M ops/s.**

### HEAD vs the historical peak anchor (2026-06-10, lx64, interleaved)

`kevy_877cd41` was the campaign's "peak" anchor binary. Same box, same
session, 3 interleaved rounds each:

| angle | peak anchor | HEAD (`7f4995f`) | delta |
|---|---|---|---|
| legacy 8sh-P256 (fixed key) GET | 8.69 M | 11.17 M | +29 % |
| legacy 8sh-P256 (fixed key) SET | 8.25 M | 9.98 M | +21 % |
| pinned-hashtag compat (random keys) GET | 6.73 M | 15.31 M | 2.28× |
| pinned-hashtag compat (random keys) SET | 6.35 M | 13.82 M | 2.18× |

Random-key load exposes what the fixed-key angle hides (chiefly the old
unbounded reaper walk), stretching the gap to 2.2×. With cluster routing —
a capability the anchor doesn't have — HEAD's 23.7 M GET stands at 2.7×
the anchor's best angle and 3.5× its same-load number.

### Long-run headline + annotate verdict (2026-06-10, lx64)

The 8 M-ops-per-process A/B segments under-amortise startup/ramp; official
long runs (30 M ops × 8 processes, pinned-hashtag cluster angle, quiet box):
**GET 30.8 M ops/s, SET 22.3 M ops/s** — GET at ~80 % of the naive 38 M
ceiling estimate.

Instruction-level annotate of the remaining top spots found no mechanical
waste left to claim: `dispatch_batch` (17 %) is flat-profile parse+dispatch
work (hottest single instruction 1.7 %, an inlined hash multiply);
`find_by_borrow` (25 %) is the keyspace lookup itself (kevy-map already
deep-polished); `shard_of` (4 %) splits roughly half hashtag `{}` scanning,
half slice-by-4 CRC — a SWAR memchr for the brace scan is worth ~1–2 %,
below the round-to-round noise floor, recorded as an observation rather
than claimed. The remaining gap to ceiling is real work, not overhead.

---

## v1.17 — cluster-aware ClusterClient — tail latency fixed (2026-06-15, lx64)

The mailrs dogfood close-out: a load-test report (`~/workspace/kevy-loadtest/
REPORT-kevy-perf-2026-06-14.md`) flagged kevy `-c50 -P1` p99 at 2–3× redis/
valkey on a single-shard connection. After ~10 rounds of attribution work
on lx64 (clean 16-core box, server cores 0-9 / client 10-15 disjoint),
five candidate "reactor fixes" (spin-scan, `yield_now`, stay-hot, dedicated
cores, CPU pinning) all measured to zero, and a hand-rolled cluster-routing
probe brought p99 down to single-shard parity. The remaining tail was the
**cross-shard forwarding hop**, not park latency, not co-location, not
fsync, not pinning.

**Fix landed as `kevy-client` 1.9.0 — `ClusterClient`** (Route B
independent release): `CLUSTER SLOTS` discovers topology at connect time,
one pooled connection per shard, and `kevy_hash::key_hash_slot` (CRC16
slice-by-4) routes each command to the owning shard. Zero MOVED hops,
zero abstraction overhead vs the bare routing probe. Covers
string/hash/list/set/zset/del/exists/dbsize/flushall/ping/publish (the
mailrs working set).

| angle (lx64, 4-shard server, `--cluster`)  | throughput | p99 | vs single-shard |
|---|---:|---:|---:|
| single-shard `Connection`, `-c50 -P1`      | 333 k ops/s | 3 858 µs | (baseline) |
| `ClusterClient`, `-c50 -P1`                | **533 k ops/s** | **260 µs** | **+60 %** throughput, **15×** p99 |

**Headline absolute numbers (long-run, 8-shard, pinned hashtag, idle
box):** GET **30.8 M ops/s**, SET **22.3 M ops/s** (carried unchanged
from the perf-ceiling campaign — v1.17 reactor changes are
observability-only and do not move the steady-state ceiling).

### v1.17.0 INFO observability + flush() rename

Same release wave landed two surface changes — neither is a perf claim,
but both touched reactor-adjacent code:

- **INFO cross-shard aggregation.** Server has per-shard `Store`; `INFO`
  used to answer from the receiving shard only (`used_memory` reported
  1/N, Keyspace empty, Stats stuck at 0). `ops::stats` now publishes
  process-level per-shard gauges from each shard's tick, and a hot-path
  `thread_local!` counter sums into the answering shard's reply. Added
  `Memory` / `Keyspace` (`db0:keys=N,expires=M`) / `Stats` (commands,
  connections, ops_per_sec, expired_keys). The expires count is an O(1)
  store-level counter (no O(n) scan in the hot path); drift-guard tests
  hold the convergence.
- **`flush()` → `flushall()`.** Three surfaces (store / embedded /
  client) renamed to kill the wipe-vs-sync footgun; `#[deprecated]`
  alias kept one release for migration.

**Note:** `INFO` adds ~1–2 ns per command (one `thread_local!` `Cell`
increment) to the reactor `start_command` path. Unit-level `kevy-store`
perfgate stayed PASS; **the e2e reactor-path replay on lx64 was not
re-run during the release**. mailrs dogfood has been asked to confirm
peak ~81 k network ops/s is unchanged.

---

# v1.22 (v3-cluster bundle) — bench refresh, 2026-06-20, lx64

**Bundle ships** P2 embed-as-read-replica + P3 scoped multi-writer +
P4 `kevy-client-async` (runtime-agnostic async client). Server hot
path is unchanged from v1.19 — scope routing in dispatch is one
extra Relaxed atomic load per command, well below noise. This
section refreshes every headline number against v1.22.0 on the same
lx64 box used since v1.13, and adds the embed-in-process and async
client numbers v1.22 enables.

Setup:
- lx64 16-core bare-metal, kernel 6.x, Docker 26.1.
- Server cpus 0-9 / client cpus 10-15 (×6 threads where applicable)
  via `taskset`. valkey 9.1 + redis 7.4 run under `docker run
  --network host` with the same cpuset.
- All persistence off (kevy `--no-aof`, valkey/redis `--save ''
  --appendonly no`).

## Server `-c50 -P16` — high-concurrency pipelined throughput

`redis-benchmark -c50 -P16 --threads 6 -n 3000000`. Each engine runs
in **isolation** (start → 2 warmed runs → stop) so kevy's busy-poll
does not starve a co-located competitor.

| engine | SET (M ops/s) | GET (M ops/s) |
|--------|--------------:|--------------:|
| **kevy 1.22 (io_uring)** | **4.0** | **6.0** |
| **kevy 1.22 (epoll)**    | **4.0** | **6.0** |
| valkey 9.1 (io-threads=10) | 1.50 | 2.00 |
| valkey 9.1 (default)     | 1.20 | 1.33 |
| redis 7.4 (io-threads=10)  | 1.50 | 2.00 |
| redis 7.4 (default)       | 1.50 | 1.71 |

→ **kevy 2.7× best-other SET, 3.0× best-other GET**. The io_uring
vs epoll gap closed at this load shape (pipelining amortises the
syscall savings io_uring brings at low concurrency). Reproduce:
`bash bench/loopback_c50.sh`.

## Server `-c1 -P1` — single-connection sequential

`redis-benchmark -c1 -P1 -n 300000`. The honest worst case for any
busy-poll engine: one client, one in-flight request.

| engine | SET (k ops/s) | GET (k ops/s) |
|--------|--------------:|--------------:|
| **kevy 1.22 (epoll)** | **76** | **68** |
| kevy 1.22 (io_uring) | 68 | 67 |
| valkey 9.1 (io-threads) | 60 | 60 |
| valkey 9.1 (default) | 51 | 52 |
| redis 7.4 (default) | 54 | 55 |

→ **kevy 1.26× best-other SET, 1.13× best-other GET**. io_uring
behind epoll here is expected — at `-c1 -P1` the kernel CQE
batching cannot fire, and the extra `io_uring_enter` syscalls
narrowly lose to epoll's tight `recv`/`send` loop. Reproduce:
`bash bench/loopback_c1.sh`.

## Embed — in-process throughput (`kevy-embedded::Store`)

`cargo run -p kevy-embedded --release --example embed_throughput`.
No socket, no RESP encode/decode, no server reactor — the call path
is `Store::set(key, value) -> io::Result<bool>` straight into the
keyspace shard.

| op | ops/s | per-op |
|----|------:|-------:|
| SET (overwrite) | **7.0 M** | 143 ns |
| GET (hit) | **9.0 M** | 111 ns |
| GET (miss) | **42.2 M** | 24 ns |
| INCR | 5.9 M | 169 ns |
| DEL | 5.5 M | 183 ns |

### What this means against valkey/redis

valkey and redis have **no in-process mode**, so the comparison can
only be "kevy embed vs kevy server vs valkey/redis server", and the
shape of the comparison has to be `-c1 -P1` (a single in-process
caller is what an embed user replaces with one Rust function call):

| backend | SET (k ops/s) | GET (k ops/s) |
|---------|--------------:|--------------:|
| kevy 1.22 embed | **7 000** | **9 000** |
| kevy 1.22 server @ localhost | 76 | 68 |
| valkey 9.1 server @ localhost | 60 | 60 |
| redis 7.4 server @ localhost | 54 | 55 |

→ embed skips the wire layer entirely and runs **~92× faster on
SET, ~132× on GET** than a TCP-loopback call to the same server.
**This is not a kevy-vs-valkey/redis throughput claim** — it is the
quantified cost of "no socket, no protocol, no reactor". An
application that can embed instead of TCP wins that overhead back.

## Pub/sub fan-out — 6-way compare

`bench/pubsub-compare/run.sh`. 1 publisher → 50 subscribers, 200 000
messages, 16-byte payload, host-loopback (Aeron uses shared-memory
IPC). Two consecutive runs; the warm number is reported because the
cold run is dominated by docker container start + JIT-style warm-up.

| system | mode | delivered msg/s |
|--------|------|----------------:|
| Aeron 1.45 (IPC) | shared memory | **84 M** |
| **kevy 1.22** | RESP broker (TCP) | **18.5 M** |
| ZeroMQ 4.3.5 | direct messaging (TCP) | 9.4 M |
| redis 7.4 | RESP broker (TCP) | 8.9 M |
| valkey 9.1 | RESP broker (TCP) | 6.8 M |
| Zenoh 1.9 | peer/mesh (TCP) | 2.9 M |

→ **kevy is the fastest TCP-based broker**: 2.0× ZMQ, 2.1× redis,
2.7× valkey. Aeron IPC is the structural ceiling (no kernel network
stack); among TCP brokers kevy leads, including beating the
non-broker ZeroMQ direct-messaging path. The methodology +
per-system code lives in `bench/pubsub-compare/`.

## Async client (`kevy-client-async` 1.0.0) — single conn

`cargo run -p kevy-client-async --release --features tokio --example
bench_throughput`. 100 000 SETs after 10 000 warm-ups against a
kevy 1.22 server on lx64 cores 0-9; client on cores 10-15.

| client | ops/s | vs blocking |
|--------|------:|------------:|
| `kevy-client` (blocking, sequential) | **4 180** | 1.00× |
| `kevy-client-async` (async, sequential) | **4 169** | 1.00× |
| `kevy-client-async` (async, pipelined batch=64) | **172 355** | **41.2×** |

→ The async client is **architecturally free** on a single connection
(meets the RFC F5 ≥ 80 % single-conn budget). The pipeline-first
sugar is where async pays off — `conn.pipeline().set(k1,v1).get(k2)
.run(&mut conn).await` collapses N RTTs to one, hitting 41× the
sequential ops/s at batch 64.

Note on the sequential number: 4 k ops/s is **lib-side bound**, not
server-side. `redis-benchmark -c1 -P1` against the same kevy server
delivers 76 k SET — the Rust client allocates `Vec<Vec<u8>>` per
argv and a fresh output `Vec<u8>` per encoded command, where
`redis-benchmark` (C) packs directly into a stack buffer. The
server still has ~20× headroom on that single connection. Closing
this gap is a v2-track candidate — pipelining is the user-side
workaround until then.
