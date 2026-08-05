# The capacity envelope: 25× holds, 40× holds, and the cost per entry is 18.7 B

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
| 40× | 21.0 M | 80 GB | **2.186 GB** | **39.3×** | 148 µs | 1.03× | 4.71 GB | **2.16×** |

Every rung passed every assertion — ratio, cold-read p99, vlog
amplification, the 14-check cold op sweep. The 10× rung reproduces
2026-07-25 almost exactly (`used_peak` 2.039 GB both times, amplification
1.27× both times), so the runs are comparable and the box has not
drifted.

## What the numbers say

**The 25× claim holds, measured.** 13.1 M keys, 50 GB of data, on a
2 GB budget: 26.3×. The vision sentence is not aspirational — it is
under-stated by the sweep, and the ceiling is above the last rung.

**The logical envelope is exact — until it is not, and the point where
it bends is the finding.** `used_memory` peaks at 2.039 GB through 10×,
20× and 25×: a 2.5× larger dataset moved it by 0.8 MB. Capacity really is
decoupled from data *volume*.

It is not decoupled from *entry count*. At 40× the peak rose to
2.186 GB — **1.8 % above the budget itself**, held inside the contract
only by its 5 % tolerance, with 69 MB of that tolerance left. Between
25× and 40× the store gained 7.9 M entries and 147 MB of resident
memory it could not demote:

> **18.7 bytes per entry, resident, non-demotable.**

That is the real capacity model, and it is a number the design docs do
not have. The window model's own estimate is ~1 B/entry — true for
*small* values, where many rows share a 4 KiB page and one fence
amortises across all of them. At 4 KiB values every row is its own page,
so every row pays its own fence, and the per-entry cost is ~19×
what the formula suggests. **The capacity claim is a function of value
size, and the sweep is at the pessimal end of it.**

**Cold-read latency is not the constraint.** 79 → 157 → 144 → 148 µs
against a 300 µs budget: it rises once and then flattens across four
rungs and an 4× dataset. The fence + bloom structure is doing its job;
reads are not what gives first.

**The RSS multiplier steps rather than plateaus.** frag 1.34× → 1.65× →
1.65× → **2.16×**. Between 20× and 25× the RSS peaks differ by 0.005 %,
which looked like a plateau and which I wrote up as one; 40× refuted
that — it is a staircase, not a ceiling. This is the glibc brk-arena
behaviour documented in
`PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md` and reclaim-proof
there (`malloc_trim` a no-op, `MALLOC_ARENA_MAX` a no-op). At 40× an
operator needs **4.7 GB of real RAM for a 2 GB budget**.

## What this means for the RDS thesis

The capacity claim can be stated, with one honest addition:

> A 2 GB budget served 80 GB of live data — 39× — with cold reads at
> 148 µs p99. Resident cost is **~19 bytes per entry** at 4 KiB values,
> and RSS runs **1.6–2.2× the budget** on this (pessimal) allocation
> pattern.

Two multipliers, two different owners:

* **Per-entry resident cost** is the *tiering* account, and it is what
  sets the true ratio ceiling. It also means the claim must be stated
  per value size — at 4 KiB the fence cannot amortise, at 256 B it can.
* **The RSS multiplier** is the *allocator* account. The v5 vision
  already names a self-built `kevy-alloc` as first priority against a
  2.24× fragmentation figure; this sweep independently reaches the same
  number from the capacity side (2.16× at 40×) and bounds the prize:
  closing it turns a 4.7 GB machine back into a 2.2 GB one.

## A prediction, written before the answer

The 60× rung (31.5 M entries, 120 GB) was launched before this section
was written, and the model above says it should **fail**: 10.5 M more
entries × 18.7 B ≈ 196 MB more resident, putting the peak near 2.38 GB
against a 2.255 GB cap. Extrapolating to where the peak meets the cap:

> **the ceiling on this workload is ≈ 47× (≈ 24.6 M entries at 4 KiB on
> a 2 GB budget)**.

If 60× passes, the linear per-entry model is wrong and this finding
needs redoing rather than extending — which is the point of writing the
prediction down first.

## Left open

* **What gives first is now known: the non-demotable per-entry
  residency.** Not reads (flat across 4× of dataset), not the vlog
  (amplification *improved* with scale — 1.27× → 1.03×), not the cold
  op sweep (14/14 at every rung).
* **One workload shape.** Bulk ingest of uniform 4 KiB values plus a
  25 % overwrite churn — which the earlier finding calls close to the
  worst case for fragmentation (*"mixed / slower real workloads fragment
  far less"*). An SME shape may never reach 1.65×.
* **The product statement is the owner's to set.** The measured forms
  available are "10× is gated" (what CI enforces), "26× measured on a
  2 GB budget", or the two-part statement above. Picking one is not
  mine — but the sentence in the master plan is now backed rather than
  hopeful.
