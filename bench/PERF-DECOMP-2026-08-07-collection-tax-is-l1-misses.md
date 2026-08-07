# The collection-write tax is L1 misses, not the tick — R4's attribution refuted by counters

Phase A of the tick-tax decomposition, run entirely on measurements the
previous rounds never took: per-call sweep counters, large-realloc copy
counters, and a perf-stat A/B. Three named suspects died and the real
account balanced to within ~3 %.

## The three eliminations (sadd angle, 60 M ops, 8 shards, ~11 s wall)

Probe: per-sweep counters (segments/spans/pages/discards) + caller-side
tick timing + large-realloc copy bytes, printed from the reclaim tick.

| suspect | measured | share of the ~12–16 % tax |
|---|---|---|
| sweep scan (R4's verdict: "the +9–12 pp hides in `thread_reclaim`") | 769 calls, **55.5 ms total**, max 1.26 ms | **0.06 %** — dead |
| discard→refault (madvise + kernel zero-fill) | 671 MB discarded over the run | ~0.6 % — dead |
| large-realloc copies (no mremap) | 75,593 reallocs, **2.74 GB copied** | ≤1 % — dead |

The realloc suspect also fails a cross-check: zadd copies **72 MB**
(38× less than sadd) yet pays the same tax class (−14.4 vs −15.8).
Copy volume and tax are uncorrelated.

**R4's conclusion — that the tax lives in the tick — is refuted by
direct measurement.** The tick fires every 100 ms (hz=10), and the
whole sweep costs three orders of magnitude too little. The LTO symbol
range R4 read as `thread_reclaim` was hiding something else (see below
— it was hiding *stalls*, which land on whatever symbol is executing).

## What the tax actually is

perf stat, identical 8 s windows, sadd storm, both binaries saturating
8 cores (cycles equal: 285.1B vs 286.3B):

| per op | OFF (glibc) | ON (kevy-alloc) | ratio |
|---|---:|---:|---|
| instructions | 9,954 | 9,718 | **0.976 — ON does *less* work** |
| branch misses | 3.11 | 0.86 | 0.28 — ON far better |
| LLC misses | ~0.00002 | ~0.00005 | both negligible (working set fits L2/L3) |
| **L1-dcache misses** | 27.2 | **75.8** | **×2.78 (+48.6/op)** |
| IPC | 1.67 | 1.48 | −11.4 % |
| cycles | 5,953 | 6,566 | **+613/op** |

Budget: +48.6 L1 misses/op at an L2-hit cost of ~12–14 cycles ≈ +630
cycles/op, against a measured +613. **The L1-miss delta explains the
entire gap, reconciling to ~3 %.**

So: the allocator executes fewer instructions and predicts better than
glibc, and loses on **data-cache locality alone**. The header-free
design pays its price here — every alloc/free walks pagemap, span
header, bitmap and claim word in cache lines far from the data, while
glibc's in-chunk metadata means the free-list touch *is* the data touch
(the header we removed was also a free prefetch). Stalls land on
whatever code is executing, which is why every profile showed inflated
*consumer* symbols — `drain_replica_inbox` +7 pp here, and pub/sub's
`deliver_publish` +6 pp, which finding
`2026-07-26-header-free-costs-a-cache-line.md` had already named — the
same mechanism, now quantified, budget-closed, and shown to be the
whole story. Collection verbs pay most because they run the most
alloc/free per op; the KV angles stay green for the same reason.

## What stays open

- The zadd **>3 s pause** is now *not* the sweep (max sweep 1.26 ms)
  and *probably not* one realloc copy (zadd's total is 72 MB); still
  unattributed — kernel-side (mmap_lock convoy on giant unmap?) is the
  remaining suspect. Separate investigation, tail-latency criterion.
- mremap for large reallocs remains a real structural gap vs glibc
  (2.7 GB of avoidable copies on sadd) but is worth ~1 % throughput at
  best — a tail/latency candidate, not the tax.

## Where the attack face is (Phase B material, next design round)

The target is **misses per op in the metadata walk**, with the §9 gate
already satisfied in aggregate (the stalls are ~10 % of cycles):
fold the hot per-op metadata touches into fewer lines (bitmap + claim
word co-residence), prefetch the span header on entry, batch bitmap
writes through the existing thread cache — or concede a measured slice
of the header-free thesis and put a free-list word back inside the
freed slot. Each candidate is a small, benchable change; per the
methodology they go to a worktree round with the perfgate collection
angles as the needle.
