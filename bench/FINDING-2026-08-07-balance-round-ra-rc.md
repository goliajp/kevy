# Balance round, first ledger: the combined state is clean, and the tail belongs to the AOF

Owner direction: pull perf/disk/stable to the best reachable balance
before the industrial pivot. Plan: `.claude/plans/2026-08-07-v5-balance-plan.md`.
This ledger banks R-A (combined baseline), R-B (the last perf knife),
and R-C.6 (rewrite interplay).

## R-A — alloc ON + compress ON, measured together for the first time

| axis | combined state | vs alloc-OFF |
|---|---|---|
| capacity envelope | full PASS: ratio 10.1×, budget held, amp 0.01×, sweep 14/14 | identical |
| cold read | scalar p99 90µs / hash-row 210µs (budgets 300/500) | 128/130 — same band |
| frag @ 4KiB values | 1.32× | 1.35× — parity (the allocator's big win is the 400B shape: 2.16 vs 2.40) |
| perfgate-median (N=3) | KV/cluster/compat/incr/lpush/set/get PASS; hset −8.7 / sadd −10.8 / zadd −14.5 | the known alloc tax; compression adds zero |
| tail (mixed small-op storm, 120s) | **PING p99.9 = 3.2ms, max 9.8ms** | drain-budget fix holds under mixed storm |

**The combined state costs nothing on disk/capacity/stability; its
only cost is the known collection-write tax.** That is the P1 decision
in one line.

## R-B — the envelope-pooling knife dies before implementation; a bigger one is named

An 85.7M-op per-class allocation census under the hset storm:
**16B ≈ 1.96/op and 32B ≈ 0.98/op — ~2.9 small allocations per op —
while every envelope/reply ladder class totals ~0.5%.** Envelope
pooling (B') has nothing to pool; dropped without writing it.

The census names the real knife instead: a 3-byte hash field value is
heap-allocated (HashData values are `Vec<u8>`; keys inline ≤22B,
values never do). **Inlining small hash values removes ~2 of the 2.9
allocations per op for BOTH builds** — a store-side change, medium
blast radius, filed as the head candidate for the alloc execution
train rather than opened here.

## R-C.6 — the rewrite is exonerated; the firehose tail is the AOF's

BGREWRITEAOF on a 2M-key tiered+compressed store under a 1KiB write
storm: the rewrite window's tail (p99.9 116ms) is statistically
identical to the no-rewrite control (123ms). Attribution grid:

| config | p99.9 | max |
|---|---|---|
| aof + tier | 116–123ms | 264–289ms |
| **no-aof + tier (full demote churn)** | **9.9ms** | **16ms** |
| aof + no tier | 161ms | 268ms |

**The 100–250ms stalls belong entirely to the AOF append path under
~1GB/s sustained ingest** (kernel writeback throttling class; fsync
cadence and group-commit interplay are the mechanism candidates).
Tiering + compression + allocator are clean at ≤10ms p99.9 under the
same pressure. Named for the tail-latency train (V3): AOF append
batching / io_uring writes / throttled group commit — an inherited
v4-era behavior under an extreme shape, not a v5 regression.

Instrument caveats, recorded: BGREWRITEAOF's completion gauge is the
answering shard's view (INFO persistence is shard-local), so "rewrite
wall 59ms" measured the wrong thing; the rewrite-duration question
needs a per-shard gauge and stays open.

## R-C.5 / R-C.7 — the compaction "mystery" is rotation granularity, and the machinery is healthy

A realistic corpus (1.5M distinct templated 400B rows, 256MB/shard
budget) with two 30% churn passes settled at **amp 1.56× with
vlog_epoch=0 — compaction never ran at ANY threshold (50/65/80)**.
The suspicion escalated to "dead accounting broken"; a local
small-scale repro with INFO gauges killed that: **vlog_live_bytes
tracks dead bytes correctly** (37.5% dead visible).

The real mechanism: 8 files on the box = **8 shards × 1 vlog each**,
and at 256MB rotation each shard's ~116MB log is entirely its ACTIVE
file — never sealed, and the active file is never compacted **by
design**. With an 8MB rotation locally the machinery proves itself
whole: churn → dead accrues → **epoch advances within 6s of ticks,
48MB → 31.2MB (amp 1.14×)**, tails unaffected (p99.9 well under 1ms
during compaction).

Balance verdicts:
- **Threshold 50 is fine**; the knob that matters is `rotate_bytes`
  (hardcoded 256MB/shard): overwrite garbage lingers up to one
  rotation's worth per shard as a *young-store transient*, not a
  steady-state leak. Product doc item + a configurability candidate;
  no code change forced.
- The identical-to-the-byte reruns that exposed this also exposed an
  instrument lesson: a `cargo build | tail -1` pipeline without
  pipefail swallowed a build failure and re-ran a stale binary —
  **results too consistent are a reason to suspect the instrument**.

## R-D.8 first soak: the veto fired in ten minutes, and it was the point

The first hour-long soak (mixed 3-byte KV/collection storm + 1KiB
tiered ingest waves, tier budget 512MB, no maxmemory) killed the
server at minute 10: **"memory allocation of 3 bytes failed" → abort,
at RSS 13.5 GB on a box with 48 GB free.**

Not OOM — a structural ceiling: `PER_CLASS_CAP`'s counter was `u16`,
so the guard that its own doc records having "raised three orders of
magnitude" was silently pinned at 65,535 spans × 64 KiB = **a hidden
4 GiB ceiling per size class**. The 3-byte storm filled the 16 B
class and the allocator refused a 3-byte request with terabytes of
address space to spare. Fixed (`7008e36f`): counter and cap are u32,
the guard sits at 1 TiB/class, and the lesson joins its first verse
in the constant's doc — memory governance belongs to maxmemory and
the tier budget, never to an invisible allocator constant.

Also recorded from the same ten minutes: the unspillable-value
boundary is real and industrial config must set BOTH knobs (tier
budget bounds spillable values; maxmemory + policy bounds the rest);
frag under the mixed small-value shape ran 2.1–3.9× (vs 1.32× at the
uniform-4KiB envelope) — the value-size dependence of the fragmentation
story, now measured from the stability side too.
