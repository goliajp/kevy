# kevy vs valkey 9.1 — baseline (v0.2)

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

# CLEAN measurement on a dedicated 16-core box (lx64) — kevy leads on all axes

**Date:** 2026-05-26. First measurement free of the three artifacts that had
depressed/distorted every prior run. Run on **lx64** (bare 16-core Linux, not a
VM), all servers **host-loopback** (no docker bridge / NAT), in-memory.

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
- **Honest caveats:** lx64 had background load (~2.7–3.8) during the runs, which
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

Harnesses: `bench/lx64_loopback.sh` (3-way -c50), `bench/kevy_ab.sh` (binary
A/B), `bench/lx64_c1.sh` (-c1). All pin server/client to disjoint cores.

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
honestly. Details: `perfs/topics/04-c50-bottleneck.md`, `05-multishot-recv.md`.

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
contradicted a standing "negligible" assumption. Data:
`perfs/data/2026-05-26/{fxhash-conn-maps,local-reply-bypass}-ab.txt`.
