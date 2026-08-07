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
