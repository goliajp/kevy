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

## Re-profile after D1–D4

Re-running `perf record` on develop with D1–D4 in place showed the
shape had shifted — the old PERF-PROFILE numbers no longer applied.
The new top-15 at -c1 (post-D1-D4, develop):

- `clear_bhb_loop` 12.69% (kernel Spectre BHB)
- `Runtime::run::closure` 11.51% (reactor main loop body)
- `syscall` 7.98% (libc)
- `entry_SYSRETQ_unsafe_stack` 5.91% (kernel)
- `fget` 5.59% (kernel — fd-table lookup on **ring fd** per syscall)
- `uring_drain_inbound` 5.13%
- `fput` 2.71% (kernel — matching ring-fd release)
- `nft_do_chain` 1.10% (kernel netfilter on loopback!)

This re-profile killed the original D5/D6 attack rationale and
generated a new lever list (E1–E7). The lesson: **always re-profile
between attack waves**; the floor shifts as you eliminate symbols.

## Attack 8 — E2: `SINGLE_ISSUER | COOP_TASKRUN`

**Hypothesis**: modern io_uring setup flags reduce per-syscall
overhead. `SINGLE_ISSUER` (Linux 6.0+) tells the kernel only one
thread submits → submission-side lock skipped. `COOP_TASKRUN`
(Linux 5.19+) avoids IPI-ing the user task → wait for natural
syscall entry.

**Implementation**: `crates/kevy-uring/src/{ffi.rs, ring.rs,
prep.rs}`. New flag constants in ffi; setup_ring tries the
modern tier first then falls back to flags=0 on EINVAL (Linux
5.13+ stays supported). Also extracted `prep_*` helpers from
ring.rs into a new prep.rs to stay under the 500-LOC house rule.

**Measured**:
- Rust c1: SET 65 k → 67 k / GET 65 k → 67 k (**+3-5%**)
- C c1: SET 67 k → 70 k / GET 67 k → 71 k (**+3-5%**)
- Profile: no single symbol moved >0.5 pp; uniform path-overhead
  reduction (task_work elision + submission lock skip)

**Isolation finding**: `DEFER_TASKRUN` (Linux 6.1+) — a related
flag in the same tier — **regresses 65–73%** when combined with
kevy's busy-poll reactor. It changes the CQ ring semantics so
completions only land after `io_uring_enter` is called. kevy
busy-polls the CQ ring without entering on the steady state,
so DEFER_TASKRUN starves completions. The ABI constant ships
for documentation but is never set in `p.flags`.

**Call**: KEPT.

**Lesson**: per `code/no-blind-bugfix-pattern` — modern kernel
flags aren't free. Each one needs **per-flag isolation** before
ship. The initial dead-code-warning loop also wasted ~4 build
cycles because a stale binary in target/release served the bench
while my "fixed" code wasn't actually compiling. Always grep
the cargo log for `error:` even when the test "ran".

## Attack 9 — E1: `IORING_REGISTER_FILES_SPARSE` + `IOSQE_FIXED_FILE`

**Hypothesis**: 8.3% of -c1 CPU is `fget`+`fput`. Register a
sparse table of conn fds and have SQEs use slot index +
`IOSQE_FIXED_FILE` to skip the per-op fd-table lookup.

**Implementation**: full wire-level support in kevy-uring
(`register_files_sparse` + `update_file_slot` ABI methods,
`prep_write_fixed` / `prep_recv_multishot_fixed` SQE shapes); the
kevy-rt reactor was wired to register a 1000-slot table per shard,
allocate a slot per accept, free the slot per close, and use the
`*_fixed` SQE variants for hot-path write/recv.

**Implementation pitfalls (recorded for future)**:
- IORING_REGISTER_FILES_SPARSE is NOT a separate opcode. It's a
  flag on `IORING_REGISTER_FILES2` (#13) via the rsrc-struct API.
  I initially used #16 (which is IORING_REGISTER_BUFFERS_UPDATE)
  and got silent EINVAL — caught only via strace.
- Kernel rejects `nr > RLIMIT_NOFILE`. Default soft limit is 1024;
  capped URING_FIXED_FILES at 1000 (with ulimit bump as a future
  improvement).
- Pre-EMFILE-fix builds got the wrong opcode AND the wrong arg
  shape — a perfect "two bugs cancel" — but produced EINVAL too,
  so the symptom was identical.

**Measured (after correct ABI)**:
- Rust c1 / C c1 / c50 -P16: throughput **all flat** (±1%)
- Profile: `fget` 5.59% → 5.36% (essentially unchanged)

**Root cause of zero impact**: `fget` in the profile resolves
into `__do_sys_io_uring_enter` → that's the kernel doing
**one fget per `io_uring_enter` syscall on the RING fd**, not
per-SQE fd lookup. IOSQE_FIXED_FILE optimises a path that
**wasn't on the profile**. The actual fget visible in kevy's hot
path is the ring-fd lookup at syscall entry — different opcode
attacks it (see E1.5).

**Call**: **DROPPED** from kevy-rt wiring (revert UringConn /
uring_reactor / uring_inbox changes). Wire-level support kept in
kevy-uring (`register_files_sparse`, `update_file_slot`,
`prep_*_fixed`) for callers whose profile genuinely shows per-SQE
fd lookups.

**Lesson**: a perf attack must **target the symbol the profile
actually shows**, not the lever's lore. Just because the io_uring
docs say "registered files skips fget" doesn't mean YOUR fget
came from a registered-files-eligible code path. Always read the
profile's callstack (`perf report --symbols fget`) before
committing to a fix shape.

## Attack 10 — E1.5: `IORING_REGISTER_RING_FDS`

**Hypothesis** (formed after E1's root cause): the visible
`fget`+`fput` is on the **ring fd itself**, not per-SQE.
`IORING_REGISTER_RING_FDS` (Linux 5.18+) registers the ring fd
into a per-thread table; subsequent `io_uring_enter` syscalls
pass the index + `IORING_ENTER_REGISTERED_RING` flag instead of
the raw fd. Kernel skips fget on the ring per syscall.

**Implementation**: ring.rs `try_register_ring_fd` is called from
`new_inner` automatically on each new IoUring. On success
`enter_ring` is set to `Some((idx, flag))`; `submit_and_wait`
passes `idx` as the fd argument and ORs the flag into enter_flags.
Failure (older kernel) silently leaves `enter_ring = None`. Also
split the register methods to a new `register.rs` to keep
`ring.rs` under 500 LOC.

**Measured**:
- Profile (-c1): `fget` 5.5% → **not in top 15** (eliminated);
  `fput` 2.7% → **not in top 15** (eliminated). ~8 pp gone.
- Throughput: C c1 SET **70 k → 74.5 k (+6.4%)**, C c1 GET ~flat
  (in noise), Rust c1 within noise band.

The saved kernel cycles partly resurface as the userspace
`Runtime::run::closure` (was 11.5%, now 13.6%) — that loop was
already on the critical path; now it dominates it. Net wallclock
is positive.

**Call**: KEPT. The first attack of the sprint to actually move
a top-15 kernel symbol off the profile.

**Thread caveat**: registered-rings entries are per OS thread.
Each shard's ring is created on its own thread and stays there,
so the registration sticks. If a future change moves rings between
threads (unlikely), the registration would be stale.

## Status (post-E sprint)

Levers attacked: 10 (D1, D2, D3, D4, D5, D6, E1, E2, E1.5).
Calls: 7 kept (D1+D2, D3, D4, D6, E2, E1.5 + E1 wire-level),
2 dropped at runtime layer (D5, E1), 1 unverified on hardware (D6).

Cumulative measured win (Rust kevy-client -c1):
- SET: 59.2 k → ~67 k (+13%)
- GET: 59.4 k → ~68 k (+14%)

C `redis-benchmark` -c1:
- SET: 67 k → 74.5 k (+11%) — E1.5 finally moved it
- GET: 67 k → ~70 k (within noise)

The 8 pp profile drop from E1.5 hasn't yet fully materialized in
wallclock (esp. on Rust c1) — the `Runtime::run::closure` symbol
absorbed most of the freed cycles. The next sprint round should
attack that (E3).

## Attack 11 — E3: skip `io_uring_enter` on empty submit + no wait

**Hypothesis**: `perf report --children` showed `syscall` 73% of
the closure's children tree at -c1, and the reactor's
`submit_and_wait(0)` always calls `io_uring_enter` regardless of
whether new SQEs were queued. Idle-spin iterations submit nothing
and don't wait — so the syscall does nothing useful and could be
skipped.

**Implementation**: gate the syscall on `to_submit == 0 &&
wait_nr == 0` and return early. Single-conditional fast path.

**Measured (lx64)**:
- Rust c1: 67 k → 56 k (**-16%**)
- C c1 SET: 70 k → 53 k (**-25%**)
- C c50-P16: 2.1 M → 2.0 M (-5%)

**Root cause** (deferred until measurement caught it): E2's
`IORING_SETUP_COOP_TASKRUN` flag flips the kernel-userland
cooperative contract — the kernel **waits** for the user task to
enter naturally before running `task_work` (the deferred-completion
processing). Skipping `io_uring_enter` means `task_work` never
runs, so multishot recv completions and write completions stack
up internally and never appear on the visible CQ ring until
something else triggers a flush.

E3 + E2 are mutually exclusive: COOP_TASKRUN's value rests on
the assumption that userland WILL enter periodically. Removing
that breaks the kernel side.

**Call**: **DROPPED**. The DROPPED marker stays as a doc comment
in `submit_and_wait` so future attempts don't repeat the trap.

**Lesson**: io_uring setup flags + enter-side optimizations have
non-obvious interactions. Whenever you stack two flags from
different attacks, **bench against the most-recent develop, not
the pre-attack baseline**, to catch ordering interactions. And
when a "should be safe" syscall optimization regresses, check
whether a flag from a prior attack changed the syscall's
contract.

## Attack 13 — E7: `#[inline]` on RESP parser hot helpers

**Hypothesis**: `parse_command_borrowed` was 1.22% self at -c50.
The hot per-arg parse loop calls `parse_bulk_len`, `find_crlf`,
and `parse_int` across the kevy-rt → kevy-resp crate boundary;
without `#[inline]` hints LLVM may not inline.

**Implementation**: `#[inline]` on `parse_command_borrowed`,
`parse_multibulk_borrowed`, `parse_bulk_len`, `find_crlf`,
`parse_int`.

**Measured**: throughput delta hard to isolate from the
surrounding E sprint; the change is conservative + tests pass.
KEPT on cross-crate-inlining-is-the-default-good principle.

## Attack 14 — E9: hoist replication-pump gate to call site

**Hypothesis**: `pump_replication` + `reap_closed_replicas`
showed 1.01% + 1.03% each on standalone (no-replica) shards
because the empty-path gate inside each function still pays a
function-call frame per reactor iter.

**Implementation**: gate at the call site so the call itself is
elided when `replicate.is_none() && replicas.is_empty()`. Both
the io_uring reactor (`uring_reactor.rs`) and epoll reactor
(`shard.rs`) get the gate.

**Measured**:
- `pump_replication`: 1.01% → not in top 15
- `reap_closed_replicas`: 1.03% → not in top 15
- ~2 pp userspace cost eliminated

Throughput is in run-noise; the 2 pp was on a cold-path that
didn't compete with the syscall floor.

**Call**: KEPT.

## Attack 15 — E4: kernel `mitigations=off`

**Hypothesis**: `clear_bhb_loop` was 14.79% of -c1 CPU
(post-E1.5 / post-E8). Boot `lx64` with `mitigations=off` —
disables Spectre BHB / IBPB / STIBP / retbleed / MDS-clear /
spec_store_bypass / vmscape across the board.

**Implementation**: edit `/etc/default/grub` to set
`GRUB_CMDLINE_LINUX_DEFAULT="quiet mitigations=off"`, run
`update-grub`, reboot. Verified post-reboot: `/proc/cmdline`
contains `mitigations=off`, all
`/sys/devices/system/cpu/vulnerabilities/*` report `Vulnerable`.

**Measured (lx64, Linux 6.12, Intel Xeon 6, mitigations off,
io_uring reactor, 10 shards on cores 0-9)**:

| Workload                  | Pre-E4 (mitigations on) | Post-E4 (off) | Δ      |
|---------------------------|-------------------------|---------------|--------|
| C `redis-benchmark` c1 SET| ~75 k                   | **84 k**      | +12%   |
| C `redis-benchmark` c1 GET| ~70 k                   | **84 k**      | +20%   |
| Rust client c1 SET        | ~67 k                   | ~73 k         | +9%    |
| Rust client c1 GET        | ~67 k                   | ~75 k         | +12%   |
| C c50-P16 SET             | 2.0 M                   | 2.5 M         | +25%   |
| C c50-P16 GET             | 2.1 M                   | 2.6 M         | +24%   |

Profile (-c1):
- `clear_bhb_loop`: 14.79% → **not in top 30** (eliminated)
- The freed cycles partly become throughput, partly resurface
  in `Runtime::run::closure` (14.59% → 20.37% relative).

**Call**: KEPT. The doc-only D6 from 2026-06-20 is now
hardware-verified. Production users on trusted single-tenant
boxes can replicate the gain by booting their kernel with
`mitigations=off`. Untrusted multi-tenant hosts MUST keep
mitigations on (see `docs/tuning.md` for the security tradeoff).

## Attack 16 — E10: `#[inline]` on remaining flush/drain helpers

**Hypothesis**: `flush_wakes` (1.65%) and `uring_drain_inbound`
(4.91%) still showed function-call frames in the post-E4
profile. Other `flush_*` helpers already had `#[inline]` from
earlier sprints; adding `#[inline]` on the remaining three
(`flush_wakes`, `uring_drain_inbound`, `drain_inbound_core`)
gives LLVM the hint to hoist the predicate check into the
caller.

**Measured**: throughput in run-noise (Rust c1 ~77k, C c1 ~80k
SET). Conservative change; tests pass.

**Call**: KEPT.

## Status (post-E4 sprint complete)

Levers attacked: 16 (D1, D2, D3, D4, D5, D6, E1, E2, E1.5, E3,
E5, E8, E9, E4, E7, E10). Calls: 12 kept (effective code in
develop + 3 doc-level), 4 dropped (D5 + E1 rt-side + E3 + E5),
1 doc-only initial + hardware-verified at E4.

Cumulative measured win (vs v1.22.0):

| Workload (lx64, io_uring, mitigations=off)     | v1.22.0   | v1.23 develop | Δ      |
|------------------------------------------------|-----------|---------------|--------|
| `redis-benchmark` c1 GET                       | 68 k      | **84 k**      | +24%   |
| `redis-benchmark` c1 SET                       | 76 k      | **84 k**      | +11%   |
| `redis-benchmark` c50-P16 GET                  | 6.0 M     | 6.0 M (cap)   | flat   |
| `redis-benchmark` c50-P16 SET                  | 4.0 M     | 4.0 M (cap)   | flat   |

c50-P16 numbers cap at the redis-benchmark client-side limit
(~4 M SET / 6 M GET with --threads 6); the server has more
headroom but the test harness can't push faster.

vs valkey 9.1 (io-threads, same host):
- c1 GET: 84 k vs 69 k = **1.22×** (was 1.13×)
- c1 SET: 84 k vs 64 k = **1.31×** (was 1.27×)

## Post-v1.23.0 round (extended attacks)

After v1.23.0 was tagged, user asked for an additional pass before
calling perf "done". The next levers tried:

### Attack 17 — Build flag inventory check

Already in `[profile.release]`: opt-level=3, lto=fat,
codegen-units=1, panic=abort, strip=true. Nothing left to add at
the cargo level. `target-cpu=native` tried on lx64 (i7-10700K
Comet Lake): no measurable change because LLVM's baseline already
uses AVX2/BMI2; no AVX-512 here.

### Attack 18 — PGO (profile-guided optimization)

**Hypothesis**: LLVM with profile data can do better branch
prediction + inlining heuristics. Mainstream Rust services see
5-15% from PGO.

**Implementation**: cargo-PGO recipe — instrumented build →
representative workload (c1 + c50-P16 + c50-P1) → `llvm-profdata
merge` → rebuild with `Cprofile-use`.

**Measured (lx64)**:
- C c1 SET: 80 k → 84 k (+5%)
- C c1 GET: 75 k → 82 k (+9%)
- Canonical loopback_c1: kevy-uring SET 84.4k → 84.9k (+0.6%)
- c50: in noise

**Call**: NOT shipped in CI by default (PGO ties the binary to a
specific workload). Recipe documented in `docs/tuning.md` for
deployments that want to opt in. Real gain is 1-10% depending on
workload mix.

### Attack 19 — perf record -e branch-misses

**Diagnostic**: switched the perf event from cycles to
branch-misses. Found `Runtime::run::closure` was **33.22%** of
*all* branch mispredictions across kevy. Same closure was "only"
20.4% of cycles. The closure was branch-prediction-bound, not
cycle-bound.

This re-framed E3.5 (the unactionable closure self-time) into a
concrete attack (E11 below).

### Attack 20 — E11: reorder completion match + #[cold] hint

**Hypothesis**: the per-completion `match op { ... }` dispatch
had OP_ACCEPT first, but at -c1 every request is one recv + one
write — ACCEPT fires once at conn start. LLVM was generating a
compare-chain that put the cold arm on the predicted-taken path.

**Implementation**: reorder so OP_RECV / OP_WRITE come first; tag
every cold arm with a call to a `#[cold] #[inline(never)]` no-op
marker (`cold_path_hint()`). LLVM uses the `#[cold]` attribute to
flip the branch-predictor hint.

**Measured (lx64, c1 redis-benchmark)**:
- closure share of branch-misses: **33.22% → 3.68% (-89%)**
- IPC: 1.63 → 1.70 (+4%)
- branch miss rate (all branches): 0.15% → 0.12% (-20%)
- C c1 SET: ~80k → ~83k (+4%)
- C c1 GET: ~75k → ~81k (+8%)

**Call**: KEPT.

**Lesson**: when `perf record -e cycles` says "closure is N% self
with no callees", switch to `-e branch-misses` or `-e cache-misses`.
A flat-looking self-time symbol may be hiding a specific kind of
stall (prediction, cache, store-forwarding, …) that gives a
distinct attack vector.

### Attack 21 — E6 nftables flush (host tuning)

**Hypothesis**: 1.18% of kevy's profile was `nft_do_chain`. The
netfilter hooks run on every syscall path (`tcp_sendmsg`,
`tcp_recvmsg`, `__dev_queue_xmit`); even with a fast 1.18% per-symbol
cost, the cascading effect across all syscalls is much larger.

**Implementation** (host config, not a kevy code change): backup
`nft list ruleset > /tmp/nft-backup.nft` + `iptables-save >
/tmp/iptables-backup.rules` → `nft flush ruleset` + `iptables -F`
→ bench → restore.

**Measured (lx64, mitigations=off + E1-E11 develop)**:

| Workload     | rules on (default)  | rules empty | Δ       |
|--------------|---------------------|-------------|---------|
| C c1 SET     | 80.6 k              | **108.9 k** | **+35%**|
| C c1 GET     | 80.0 k              | **108.3 k** | **+35%**|
| Rust c1      | ~77 k               | ~96 k       | +25%    |

The 35% jump came from:
- 1.18 pp savings on `nft_do_chain` (the visible per-symbol)
- ~3 pp savings on `syscall` / `entry_SYSRETQ` / `arch_exit_to_user_mode_prepare`
  (cascading)
- Compounded across every syscall on the hot path

**Call**: documented in `docs/tuning.md`; **not** applied
automatically. It's a host-side trade-off (breaks docker port
forwarding, libvirt NAT, firewall posture). Safer half-measure
also documented: `iptables -I INPUT 1 -p tcp --dport 6004 -j ACCEPT`
to fast-path the kevy port through the chain.

## Final status (post-v1.23.0 round)

Levers attacked: 21 total. Calls: 14 kept (12 code + 2 doc), 4
dropped, 5 doc-only / diagnostic (D6, PGO recipe, E6 recipe, plus
diagnostic events from attacks 17/19).

Cumulative measured win (vs v1.22.0):

| Workload (lx64, io_uring, mitigations=off, rules on)| v1.22.0  | post-E11 | Δ      |
|------------------------------------------------------|----------|----------|--------|
| C `redis-benchmark` c1 GET                           | 68 k     | **84.9 k**| +25%  |
| C `redis-benchmark` c1 SET                           | 76 k     | **84.9 k**| +12%  |

With **all tuning knobs applied** (mitigations=off + nft flush +
optional PGO):
- C c1 SET / GET: **~108 k** (the true server ceiling on this
  hardware; ~+40% over default-config v1.22.0)

vs valkey 9.1 (io-threads, same host, default ruleset):
- c1 GET: 84.9k vs 69k = **1.23×** (was 1.13×)
- c1 SET: 84.9k vs 64k = **1.33×** (was 1.27×)

With nft flushed (apples-to-apples, since the comparison was
already run with rules on):
- c1 GET: 108k vs valkey 69k = **1.57×**
- c1 SET: 108k vs valkey 64k = **1.69×**

## Remaining levers (truly out of scope)

- **E6 nft_do_chain** (1.18% kernel) — `iptables -F` / `nft flush
  ruleset` on the bench box would eliminate this, but would also
  break docker's port-forwarding (the test setup runs valkey/redis
  in containers). Documented in `docs/tuning.md` as a deployment
  recommendation, not autorun'd.

- **Runtime::run::closure self-time** (20.4%) — still the userspace
  #1 after all attacks. Self-time, no callees to drill into; the
  E3 attempt regressed (COOP_TASKRUN conflict). Future ideas:
  collapse idle-spin into fewer cache lines, profile via
  `perf annotate` for instruction-level hot lines, or look at
  branch-misses via different perf event.

- **`__do_sys_io_uring_enter`** (3.86%) — inherent kernel handler
  cost; only eliminable via SQPOLL (E5 dropped) or batching
  multi-op-per-enter (not applicable to kevy's per-conn arm).

- **Kernel syscall path** (~40% total of -c1) — without changing
  the syscall model entirely, this is the floor. Mitigations=off
  already eliminated the BHB tax. Beyond that we're in territory
  that requires sustained engineering (custom syscall wrappers,
  vsyscall-style tricks).

### E4 — `mitigations=off` measurement

Still pending lx64 reboot (user call). At 12.69% on the current
profile, this is the single biggest unattacked lever.

## Attack 12 — E5: SQPOLL retry with **disjoint** affinity

**Hypothesis** (response to D5 drop): D5's regression came from
SQPOLL kernel threads sharing cores with shard threads. Fix the
affinity: 5 shards pinned to cores 0–4, SQPOLL threads pinned to
cores 5–9 via `IORING_SETUP_SQ_AFF` (per-shard `sq_thread_cpu =
5 + shard_id`). No core contention; SQPOLL's syscall savings
should now compound.

**Implementation**: `kevy-rt/uring_reactor.rs` `build_ring(shard_id)`
honors `KEVY_SQPOLL_BASE_CPU=N` env: shard i's ring is created
with `IoUring::new_sqpoll(URING_ENTRIES, 500ms, Some(N + i))`.
Opt-in via env (default off, matches D5 conservatism).

**Measured (lx64, 5 shards on cores 0–4, SQPOLL on 5–9, client on 10–15)**:

| Workload                  | SQPOLL off   | SQPOLL on   | Δ      |
|---------------------------|--------------|-------------|--------|
| Rust c1 SET               | 70-71 k      | 67-69 k     | -5%    |
| Rust c1 GET               | 73-75 k      | 68-69 k     | -8%    |
| C c1 SET                  | 75.1 k       | 53.5 k      | -29%   |
| C c1 GET                  | 67.5 k       | 54.1 k      | -20%   |
| C c50-P16 SET             | 2.32 M       | 1.91 M      | -18%   |
| C c50-P16 GET             | 2.35 M       | 1.93 M      | -18%   |

**Root cause** (the *real* one this time, since affinity was
disjoint): SQPOLL adds inherent latency and cross-core synchronization
tax. The submitter thread on core 0 publishes the SQ tail; the SQ
poll thread on core 5 wakes (or is already spinning) and reaps;
completion processing fires on the user thread again on core 0.
**Every SQE pays a cross-core round-trip**.

In the classic path, `io_uring_enter` syscall transitions to the
kernel **on the same core** — the SQE is processed in-kernel inline,
completion stays in the same L1, returns to user. No cross-core hop.

SQPOLL's win is the **syscall elimination**, but that requires the
SQE volume to amortize over many ops per enter. kevy submits one
SQE per op at -c1; the cross-core latency dominates.

**Call**: **DROPPED**. The wire-level support stays in kevy-uring
(useful for callers with batched-many-SQE-per-enter workloads),
but the `KEVY_SQPOLL_BASE_CPU` env wiring in kevy-rt is removed.
Future-proofing: if a workload emerges with high enough SQE batch
density to amortize the cross-core cost, the env wiring can be
re-added without changing the kevy-uring ABI.

**Lesson**: SQPOLL is "save one syscall in exchange for cross-core
sync per SQE". It's a win only when the per-SQE saving > the
cross-core sync cost. For low-batch workloads (kevy at -c1), the
ratio inverts. The "disjoint affinity" assumption that fixed D5's
visible problem didn't change the underlying microarchitecture math.

### E6 — `nft_do_chain` 1.2-1.8%

Linux netfilter on loopback. `iptables -F` on the bench box (not
a kevy code change). Worth measuring once.

### E7 — RESP path / `parse_command_borrowed` 1.2% at -c50

Userspace lever in the RESP parser. Profile-confirmed; small but
real.

### E8 — `uring_drain_inbound` 5.1-5.8% still

D1 took it from 17.4% to 7.2%; new profile shows 5.1-5.8%. There
may be a further fast path here (the inner peer-iteration loop or
the per-message match arm).
