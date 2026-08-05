# The capacity envelope: 25× holds, and the RSS multiplier plateaus

Blocker 1 of the v5 RDS ledger — *"the capacity claim has no number"*.
It has one now, and it is better than the claim.

## First, a correction

The ledger said the capacity claim had **never been measured
end-to-end**. That was wrong: the bench box carries a **full-scale run
from 2026-07-25** with all six tiergate lines green and cold segments on
real disk (the runner refuses a tmpfs data dir by design).

The real gap was sharper than the one I wrote down:

> That run reports `ratio=10.0x/10x`. It passed at exactly the ratio it
> was **sized for** — 5 M × 4 KiB on a 2 GB budget *is* 10×. It says the
> contract holds at 10×. It does not say where the engine stops.

And the sentence it underwrites — *"this 8 GB machine holds 200 GB"* —
is **25×**. Nobody had measured the 2.5× between them.

## The sweep

Hold the budget **fixed** and grow the dataset. That is the shape of the
claim (a machine of a given size, more business on it), and it loads the
part of the model that scales with **entry count** — per-entry index
metadata, fences, blooms — rather than with bytes. Shrinking the budget
against a fixed dataset would raise the ratio without ever asking that
question.

`bench/capacity-ceiling-sweep.sh`, lx64, 8 shards, 4 KiB values, 2 GB
budget, cold segments on NVMe:

| rung | keys | data | `used_memory` peak | data:RAM | cold p99 | vlog amp | RSS peak | frag |
|---|---|---|---|---|---|---|---|---|
| 10× | 5.24 M | 20 GB | 2.039 GB | **10.5×** | 79 µs | 1.27× | 2.74 GB | 1.34× |
| 20× | 10.5 M | 40 GB | 2.039 GB | **21.1×** | 157 µs | 1.11× | 3.36 GB | 1.65× |
| 25× | 13.1 M | 50 GB | 2.039 GB | **26.3×** | 144 µs | 1.14× | 3.36 GB | 1.65× |

Every rung passed every assertion — ratio, cold-read p99, vlog
amplification, the 14-check cold op sweep. The 10× rung reproduces
2026-07-25 almost exactly (`used_peak` 2.039 GB both times, amplification
1.27× both times), so the runs are comparable and the box has not
drifted.

## What the numbers say

**The 25× claim holds, measured.** 13.1 M keys, 50 GB of data, on a
2 GB budget: 26.3×. The vision sentence is not aspirational — it is
under-stated by the sweep, and the ceiling is above the last rung.

**The logical envelope is exact.** `used_memory` peaks at 2.039 GB
against a 2 GB budget at *every* rung — 2.5× the dataset moved it by
0.8 MB. Capacity really is decoupled from data volume; the budget is the
budget.

**Cold-read latency is not the constraint.** 79 → 157 → 144 µs against a
300 µs budget. It rises from 10× to 20× and then stops rising: the fence
+ bloom structure is doing its job, and reads are not what will give
first.

**The RSS multiplier rises and then plateaus.** frag 1.34× → 1.65× →
1.65×; the RSS peaks at 20× and 25× differ by 0.005 % (3 361 005 568 vs
3 360 829 440 bytes). This is the glibc brk-arena behaviour documented in
`PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md` —
reclaim-proof there (`malloc_trim` a no-op, `MALLOC_ARENA_MAX` a no-op).
What the sweep adds is that it is **bounded, not cumulative**: it is a
high-water mark set by the demotion churn's allocation pattern, not a
leak that grows with entry count. I expected the opposite and was wrong.

## What this means for the RDS thesis

The capacity claim can be stated, with one honest addition:

> A 2 GB budget served 50 GB of live data — 26× — with cold reads at
> 144 µs p99 and the accounting bound held exactly. Provision **~1.65×
> the budget in real RAM**: an 8 GB budget wants a 13 GB machine.

That multiplier is the only gap between the claim and the hardware, and
it is an **allocator** number, not a tiering one. The v5 vision already
names a self-built `kevy-alloc` as first priority against a 2.24×
fragmentation figure; this sweep says the same thing from the capacity
side, and bounds the prize: closing it turns a 13 GB machine back into
an 8 GB one.

## Left open

* **The ceiling is still above the sweep.** 40× and 60× are running.
  Every rung so far held the logical bound exactly, so what gives first
  is genuinely unknown.
* **One workload shape.** Bulk ingest of uniform 4 KiB values plus a
  25 % overwrite churn — which the earlier finding calls close to the
  worst case for fragmentation (*"mixed / slower real workloads fragment
  far less"*). An SME shape may never reach 1.65×.
* **The product statement is the owner's to set.** The measured forms
  available are "10× is gated" (what CI enforces), "26× measured on a
  2 GB budget", or the two-part statement above. Picking one is not
  mine — but the sentence in the master plan is now backed rather than
  hopeful.
