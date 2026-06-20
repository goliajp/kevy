# Perf attack log — v1.22.x sprint, 2026-06-20

Companion to [`bench/PERF-PROFILE-2026-06-20.md`](PERF-PROFILE-2026-06-20.md).
Records each lever attacked, the measured ops/s + perf-symbol
change, and the call (kept / dropped / followup).

## Baseline (post-io_uring-nap-fix, no other levers)

Rust kevy-client `Connection`, single-conn sequential, N=100k:
- SET: **59 192 ops/s**
- GET: **59 436 ops/s**

C `redis-benchmark -c1 -P1`, N=300k:
- SET: ~76 k / GET: ~68 k

Hot symbols (Rust -c1, see PROFILE file):
- `uring_drain_inbound` 17.36%
- `Runtime::run::closure` 12.93%
- `clear_bhb_loop` 10.45% (kernel Spectre BHB)
- syscall + kernel ~32%
- `flush_wakes` 2.58% + `flush_backlog` 2.49% (fast-path early-return cost)
- actual command work (RESP + dispatch + Store) ~0.3%

## Attack 1 — D1: conditional `drain_inbound`

**Hypothesis**: drain_inbound sweeps N empty cross-shard rings every
reactor iter even at -c1. Add a u64 dirty bitmap (`inbound_dirty[me]`),
senders OR a bit after pushing, drain swap+iterates set bits.

**Implementation**: `crates/kevy-rt/src/{shard.rs, runtime.rs,
shard_flush.rs, inbox.rs}`. New `inbound_dirty: Vec<Arc<AtomicU64>>`
field, Release/AcqRel pair for cross-shard visibility. Loom tests
pass.

**Measured**:
- `uring_drain_inbound`: 17.36% → **7.20%** (−58%)
- Rust -c1 SET: 59 192 → **62 367** (+5%)
- Rust -c1 GET: 59 436 → **64 949** (+9%)

**Call**: KEPT. Real win.

## Attack 2 — D2: u64 bitmap fast-path for `pending_wakes` / `backlog_nonempty`

**Hypothesis**: the early-return checks in `flush_wakes` and
`flush_backlog` were N byte/struct loads each. The fast-path itself
showed up as 2.5% × 2 = 5% of -c1 CPU.

**Implementation**: replace `pending_wakes: Vec<bool>` with `u64`
(bit per dst); add `backlog_nonempty: u64` co-maintained by send_to
(set on spill) and flush_backlog (clear when drained). Early-return
becomes a single `!= 0` load.

**Measured**:
- `flush_wakes`: 2.58% → <1.29% (off top-15; -50% relative)
- `flush_backlog`: 2.43% → <1.29% (off top-15; -47% relative)
- Rust -c1 SET: 62 367 → **63 798** (+2% over D1)
- Rust -c1 GET: 64 949 → **66 441** (+2% over D1)

**Call**: KEPT.

### D1+D2 cumulative win

- Rust -c1 SET: **+8% (59 → 64 k)**
- Rust -c1 GET: **+12% (59 → 66 k)**
- vs valkey/redis (same Rust caller): kevy SET 1.16×, GET 1.20×
  best-of-rest (was 1.02× / 1.04× pre-D1)
- C `redis-benchmark -c50 -P16`: unchanged (4.0M SET / 5.9M GET).
  High-concurrency workloads never enter the empty-housekeeping
  paths the bitmaps optimise.

## Attack 3 — D4: client zero-alloc `request_borrowed`

**Hypothesis**: per-call `Vec<Vec<u8>>` argv + `Vec<u8>::new()`
encode buffer was an alloc tax even at -c1. Predicted +15-30% Rust
caller win.

**Implementation**:
- `kevy-resp`: new `encode_command_borrowed<A: AsRef<[u8]>>(out,
  args: &[A])` (generic over `&[Vec<u8>]`, `&[&[u8]]`, etc.)
- `kevy-resp-client`: `RespClient.write_buf: Vec<u8>` reused across
  requests; new `request_borrowed(&mut self, args: &[&[u8]])`.
- `kevy-client::Connection`: 20+ hot methods (set, get, incr,
  persist, ping, dbsize, flushall, ttl_ms, type_of, publish, hget,
  hlen, hgetall, hkeys, hvals, llen, smembers, scard, sismember,
  zscore, zcard) converted to `request_borrowed(&[b"VERB", key,
  ...])` stack arrays.

**Measured**:
- No alloc symbols in top-15 hot list before OR after — client
  alloc was never on the profiled hot path.
- Rust -c1 SET: 63 798 → **62 260** (-2%, noise band)
- Rust -c1 GET: 66 441 → **67 207** (+1%, noise band)
- C `redis-benchmark`: unchanged (D4 doesn't touch C client).

**Call**: KEPT for API improvement (zero-alloc surface is a real
ergonomic win for downstream callers; e.g. future Rust client
pipelining will reuse the same `request_borrowed` shape), but
**throughput impact is null**. The prediction "client alloc is a
measurable per-op tax" was refuted by the perf profile — alloc
cost was below the 1% top-15 cutoff.

**Lesson**: even after the profile-driven re-estimate, I
over-estimated this one. The real hot floor at -c1 is the
kernel/syscall path + reactor open-loop overhead, which D4 cannot
touch. The post-profile rule must be **stricter**: only attack
things visible in the profile's top-15.

### Connection methods NOT yet converted (scope kept)

`Connection::del / exists / set_with_ttl / incr_by / expire / mget
/ mset` still use the `Vec<Vec<u8>>` + .to_vec() builder pattern
because their argv length is variable or they need int-to-bytes
conversion. Same for `ClusterClient` (all methods, ~14 sites) and
`Transaction` (~3 sites). These would need either:
- a stack-friendly itoa helper for int args, plus
- a `Vec<&[u8]>` argv builder reused per call (lifetime fiddly), or
- a typed pipeline-style builder.

Deferred to D4.5 if user wants. The profile says the gain is
small.

## Attack 5 — D3: bitmap fast-path for `request_batch` / `publish_batch`

**Hypothesis**: `Runtime::run::closure` 13% self-time included two
N-shards `is_empty()` sweeps (`flush_requests` + `flush_publish`) on
every reactor iter, even in the steady-state -c1 case where only one
or zero target shards have queued work. Same shape as D1/D2:
maintain a u64 `*_nonempty` bitmap set at push sites, short-circuit
flushers on `== 0`, trailing_zeros-iterate only set bits.

**Implementation**: `crates/kevy-rt/src/{shard.rs, runtime.rs,
exec.rs, exec_pubsub.rs, exec_dispatch.rs, exec_watch.rs}`.
Push sites: `exec_dispatch.rs:79`, `exec_watch.rs:363`,
`exec_pubsub.rs:177`. Flushers: `exec.rs::flush_requests`,
`exec_pubsub.rs::flush_publish`.

**Measured**:
- `Runtime::run::closure` self-time: **13.3% → 10.4%** (−2.9 pp)
- Rust -c1 SET: 62 367 → ~65 000 (+4%, within noise band 64–66 k)
- Rust -c1 GET: 64 949 → ~65 000 (within noise)

**Call**: KEPT. Profile-confirmed reduction is real and there is no
regression; wallclock gain is absorbed by the kernel/syscall floor
(see D5/D6) so it doesn't show up as ops/s today. Same shape as D4:
the lever is real on the profile and will become visible if the
syscall floor moves.

**Lesson**: -c1 at this point is so deep into the floor that even a
2.9 pp drop on the top userspace symbol doesn't move ops/s — D5/D6
must land first to unmask further userspace wins.

## Attack 6 — D6: Spectre `mitigations=off` documentation

**Hypothesis**: `clear_bhb_loop` is the largest single CPU consumer
in the -c1 profile (13.35%), more than any kevy userspace symbol.
Booting with `mitigations=off` on trusted single-tenant boxes
removes it entirely.

**Implementation**: `docs/tuning.md` + ja/zh-CN translations.
Documentation only — no code change. Wires a pointer from each
README's Performance epilogue.

**Measured**: Not yet — requires a reboot of the lx64 reference,
which is the user's call (lx64 is a shared perf box). Predicted:
SET 65 k → ~75 k, GET 65 k → ~75 k at -c1.

**Call**: KEPT (doc only, zero risk to ship).

## Attack 7 — D5: `io_uring` SQPOLL feature flag

**Hypothesis**: kernel polls SQ — eliminates `io_uring_enter`
syscall per op. The kernel/syscall bucket is ~60% of -c1 CPU.
Predicted gain: 1.5–2× at -c1, opt-in via `KEVY_SQPOLL=1`.

**Implementation**: wire-level `IoUring::new_sqpoll(entries,
idle_ms, cpu)` in `kevy-uring/ring.rs` with `IORING_SETUP_SQPOLL`
flag, `sq_flags` mmap'd cursor, and a `submit_and_wait` fast path
that skips `io_uring_enter` when `IORING_SQ_NEED_WAKEUP` is clear
+ caller doesn't need to block. `KEVY_SQPOLL=1` env wired in
`kevy-rt/uring_reactor.rs::build_ring`.

**Measured** (lx64, 10 shards on 16 cores, KEVY_IO_URING=1):

| Workload         | SQPOLL off       | SQPOLL on       | Δ      |
|------------------|------------------|-----------------|--------|
| Rust -c1 SET     | ~67 k ops/s      | ~4 k ops/s      | -94%   |
| Rust -c1 GET     | ~64 k ops/s      | ~5 k ops/s      | -92%   |
| redis-bench -c1 SET | 66.2 k       | 19.9 k          | -70%   |
| redis-bench -c1 GET | 67.2 k       | 29.2 k          | -57%   |
| redis-bench -c50-P16 SET | 2.05 M  | 922 k           | -55%   |
| redis-bench -c50-P16 GET | 2.10 M  | 942 k           | -55%   |

**Root cause**: SQPOLL spawns one kernel poll thread *per ring*.
kevy's per-shard ring layout means N shards → N kernel poll
threads, each spinning at 100%. On lx64 (16 cores, 10 shards),
the 10 `iou-sqp-*` kernel threads ended up on cores 0, 3, 9 —
the **same** `taskset -c 0-9` the user shard threads were pinned
to. CPU contention halves effective shard CPU and adds scheduler
noise; the 2–15× regression is the consequence.

**Architectural mismatch**: SQPOLL is designed for
single-threaded reactors that offload submission to a kernel
thread (think a single user thread + 1 spare core). kevy's
shared-nothing thread-per-core layout already saturates each
core; adding a per-shard kernel poll thread halves it. The math
would require **2× cores than shards** to host SQPOLL without
contention — kevy's defaults assume exactly the opposite.

**Call**: **DROPPED**. The wire-level support stays in
`kevy_uring::IoUring::new_sqpoll` (it is correct code, useful for
future callers with a single-threaded reactor), but the
`KEVY_SQPOLL` env in `kevy-rt` is removed. A user who runs `kevy`
with N shards on a host with ≥ 2N cores could still wire SQPOLL
in a custom integration, but the default path will never enable it.

**Lesson**: profile-confirmed savings (in this case the predicted
syscall-floor reduction) can be eclipsed by second-order
architectural cost. The right test for "ship this knob" is
end-to-end ops/s on the real layout, not the theoretical syscall
delta. A future re-attempt would need an "exclusive subset of
shards uses SQPOLL with affinity to disjoint cores" mode, which
is a bigger architectural change than this sprint scoped.

## What's left in the lever list

### D4.5 — finish client zero-alloc surface

If a future lever moves the syscall floor (a new io_uring API
shape, or `mitigations=off` ships on prod), client-side per-op
cost becomes proportionally more visible. Revisit then.

### Re-profile after `mitigations=off` is verified

Once the lx64 box is rebooted with `mitigations=off` and the
predicted -12 pp kernel drop materializes, re-run the perf
flamegraph to find the next top symbol. The current top-15 are
all kernel/io_uring path; the next ones may be userspace levers
worth attacking.

## Status

Levers attacked: 6 (D1, D2, D3, D4, D5, D6). Calls: 5 kept, 1
dropped (D5). D6 is doc-only, untested on hardware until user
reboots lx64.

Merged to develop: ce28b92 (D1+D2), 4de21fd (D4), b71f788 (D3),
plus this branch (D5 partial + D6 + log).

Cumulative measured win (Rust kevy-client -c1):
- SET: +10% (59.2 → ~65 k ops/s)
- GET: +10% (59.4 → ~65 k ops/s)
- vs same Rust caller against valkey/redis: kevy lead grew from
  ~1.04× to ~1.15–1.20×

C `redis-benchmark`: untouched (D1–D4 don't touch C client).
