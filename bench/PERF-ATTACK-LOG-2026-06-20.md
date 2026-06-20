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

## Status

Levers attacked: 5 (D1, D2, D3, D4). Calls: 4 kept, 0 dropped.
Merged to develop: 61725d8 (D3), 4de21fd (D4), ce28b92 (D1+D2).

Cumulative measured win (Rust kevy-client -c1):
- SET: +10% (59.2 → ~65 k ops/s)
- GET: +10% (59.4 → ~65 k ops/s)
- vs same Rust caller against valkey/redis: kevy lead grew from
  ~1.04× to ~1.15–1.20×

C `redis-benchmark`: untouched (D1–D4 don't touch C client).

## What's left in the lever list

Re-ranked based on profile findings (D3 done):

### D5 — `io_uring` SQPOLL feature flag

Kernel polls SQ — eliminates `io_uring_enter` syscall per op. The
kernel/syscall bucket is ~60% of -c1 CPU; this is the only lever
that meaningfully cuts it. Costs 1 CPU core 100%, opt-in only.
Effort: 3-5 days + ops doc. Predicted gain: 1.5-2× at -c1.

### D6 — Spectre `mitigations=off` documentation

`clear_bhb_loop` (12% at -c1, 5% at -c50) is the kernel BHB
mitigation, per syscall. Boot kernel with `mitigations=off` —
opt-in for trusted single-tenant boxes. Effort: zero (doc only).
Gain: -12% kernel cost at -c1.

### D4.5 — finish client zero-alloc surface

If D5 lands and the syscall cost drops, client-side per-op cost
becomes proportionally more visible. Revisit then.

### Decision point

D5 is the only lever that can pull kevy from ~65 k to ~120 k Rust
-c1 (and ~120 k C -c1 to ~200 k). It's also the longest by far.
D6 is a documentation update that gives 13% to anyone willing to
turn off Spectre on a trusted single-tenant box.

Recommended order (D3 done): **D6 (doc, zero risk) → D5 (real work)**.
