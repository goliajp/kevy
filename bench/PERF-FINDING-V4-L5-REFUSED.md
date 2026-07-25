# T9 L5 finding — the io_uring feature basket is empty; every surface was already taken

Verdict: all three L5 sub-items (direct file descriptors, RECVSEND_BUNDLE,
MSG_RING) are REFUSED at the pre-Phase-B gate. Zero code changed; the
knife never landed on any of them. Each premise was measured
independently on lx64 (kernel 6.12.90, release-perf at 05236c1b,
server cores 0-7 `--threads 8 --no-aof`, client cores 8-15, the T9
main-axis shapes) and each lands 4x to six orders of magnitude under
the 2pp gate bar.

## Gate instruments

1. **fd-symbol perf record** (clean binary): 12 s `perf record -F 499
   -C 0-7` under continuous c50 P16 load, three shapes (GET / SET /
   get+set mix), fd-lookup kernel symbols summed from `perf report`.
2. **Counter probe** (uncommitted, reverted after the run — same
   protocol as the L3 gate): relaxed-atomic counters at the recv-CQE
   arm (`c.res >= PBUF_SIZE` flags a full 16 KiB buffer), the
   waker-pipe send (`flush_wakes_slow`), `uring_park`, the OP_WAKER
   arm, and the nap rung; 5 s cumulative prints. Windows: 12 s idle,
   then 25 s each of c50 GET, c100 GET, c50 SET, plus a bigval
   `-d 65536` SET window for the BUNDLE reopen question.

## Sub-item 1 — direct fds (IORING_REGISTER_FILES): REFUSED, <0.5pp

| shape | fget | fput | io_file_get_normal | io_file_get_flags | sum |
|---|---:|---:|---:|---:|---:|
| c50 GET | 0.26% | 0.09% | 0.06% | 0.05% | **0.46pp** |
| c50 SET | 0.25% | 0.05% | — | — | **0.30pp** |
| c50 mix | 0.26% | 0.05% | 0.05% | 0.05% | **0.41pp** |

The decomp's "省 per-op fget/fput" premise assumed a per-op surface
that no longer exists: E1.5 (`c910d9b5`, v1.23 era) already registered
the ring's own fd (`IORING_REGISTER_RING_FDS`) — that was the 8pp
fget/fput block — and multishot recv holds its file reference across
completions, so no recv CQE ever pays an fget. What remains is one
`fget`/`fput` pair per **write SQE** (~1 per conn per iter, ~400k/s
against 6.4M ops/s), and that whole surface measures 0.30-0.46pp.
The kevy-uring stone already ships the full fixed-file API
(`register_files_sparse` / `prep_write_fixed` /
`prep_recv_multishot_fixed`, `register.rs` / `prep.rs`) — wiring it
into the reactor buys at most half a point gross, before paying the
fd-slot lifecycle (accept -> update -> close-time unregister) on a
conn-churn-sensitive path (axis K).

## Sub-item 2 — IORING_RECVSEND_BUNDLE: REFUSED, merge surface = 0 on the main axis

Counter probe, steady-state windows:

| shape | recv CQE/s | full-16KiB CQEs | ops/CQE |
|---|---:|---:|---:|
| c50 GET | 397k | **0** of 30.7M (whole run) | ~17 |
| c100 GET | 449k | 0 | ~18 |
| c50 SET | 380k | 0 | ~15 |
| bigval -d 65536 SET | 246k | **96.1%** | 4.13 CQE/op |

- **Main axis (-d 3)**: BUNDLE merges multiple provided buffers into
  one CQE — a surface that exists only when one completion drains
  >= 16 KiB. Across the entire probe run (30.7M recv CQEs) that
  happened exactly **zero** times: a 16-op pipeline burst is ~600 B.
  Distinct TCP arrivals post distinct multishot CQEs with or without
  BUNDLE, so there is nothing to merge. The residual per-CQE fixed
  cost (~90-150 cycles by the L3 gate's accounting) at 397-449k CQE/s
  is 0.12-0.22% of the cycle budget — total elimination is 10x under
  the bar.
- **Bigval (-d 65536)**: the merge surface is real (a 64 KiB body
  arrives as 4x16 KiB CQEs; BUNDLE could fold ~3.1 CQEs/op away). But
  the pie is ~465-1,085 cycles/op (~0.12-0.29 µs) on an axis the
  v1.29 campaign proved kernel-TCP-bound end to end — eliminating
  6-12 µs/op of userspace memcpy there was throughput-neutral
  (PERF-FINDING-2026-06-29, Finding #2). A candidate 20-50x smaller
  than a proven-neutral attack is not a lever.

## Sub-item 3 — MSG_RING (replace the waker pipe): REFUSED, ~0.0001%

Counter probe, wakes actually sent (`parked[target]` was true) under
full load:

| shape | wake_sent/s | waker CQE/s | parks/s |
|---|---:|---:|---:|
| idle | 0 | 0 | 160 (50 ms tick timeouts) |
| c50 GET | 1.4 | 0.6 | 81 |
| c100 GET | 2.8 | 0.9 | 156 |
| c50 SET | 1.3 | 0.6 | 70 |
| bigval SET | 6.6 | 6.6 | 130 |

The wake path only fires when the target shard is parked, and under
load shards spin or nap instead of parking (the L2-validated ladder).
At 1.4-6.6 wakes/s, even costing the full self-pipe round trip
(write(2) + OP_WAKER CQE + drain read, ~10k cycles) the surface is
~30-70k cycles/s against a 30.4G cycles/s budget — **~0.0001-0.0002%**,
six orders of magnitude under the gate. MSG_RING is dragonfly's
answer to a fiber-scheduler wake rate kevy does not have.

## Corroborating residue (archived, no action)

- The nap rung fired 5,057/s during c50 GET (2.9k/s c50 SET, 930/s
  c100) — 8 shards spending ~12% of wall time in 200 µs aggregation
  naps at c50. This is the S19 "shape of waiting" the L2 verdict
  already ruled productive; the probe independently reproduces it.
- c50 GET bimodality reproduced again (6.39M x2 / 7.98M x1 within one
  window; c100 sat at 7.98M) — consistent with the L3 re-measure and
  the L2 client-side-regime re-read.
- `SUBMIT_ALL` and nft offload (the basket's tail items) were not
  separately gated: `SUBMIT_ALL` changes behavior only on submission
  error paths (kevy submits via one `submit_and_wait` per iter and
  the enter syscall glue is ~150 c/op total), and nft is box
  configuration, not kevy code — both are below the smallest gated
  item by inspection of the same profiles.

## The basket verdict closes the T9 lever table

L1 REFUSED (seqlock prototype gate, 5-10x short of the 0.3 µs bar) ·
L2 REVERTED (blocking-enter redesign measured flat-to-negative; the
ladder is productive waiting) · L3 REFUSED (zero conn-duplicate CQEs
in 33M; the kernel already batches) · L4 REFUSED (huge pages already
engaged; the whole DRAM-stall pie is 5.7pp) · **L5 REFUSED (this
gate: 0.46pp + 0 merge surface + 0.0001%)**. The T9 table is closed
with zero landed code changes and five measured refusals; the v4 arc
moves off the perf axis.
