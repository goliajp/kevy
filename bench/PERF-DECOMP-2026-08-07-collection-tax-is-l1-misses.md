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

## Phase B, first intervention — and the budget model's own correction

Sampling on the *miss event* (not cycles) found the misses were not
where the self-time profiles pointed: **one inlined atomic load
(`atomic.rs:3904`) inside `drain_replica_inbox`'s symbol range took
27.25 % of ALL L1 misses**, and the allocator walk
(`Heap::alloc`/`pop_slot`/`dealloc`) another ~25 %. Source read: the
server creates a replica inbox for every shard unconditionally, so
every reactor iteration paid one `Vec::with_capacity(64)` allocation,
one idle-mpsc probe (the atomic), and one release store — millions of
times a second across 8 shards, with the eight signals' atomics packed
onto shared cache lines by the allocator's dense startup layout
(glibc's chunk headers happened to pad them apart — placement luck,
not design).

The fix (flag-gated early return — the wake contract already
guarantees a sender raises the flag; a capped drain re-raises it —
plus `#[repr(align(64))]` on the signal):

- **Mechanism: confirmed surgically.** L1 misses 3.31 B → 1.59 B per
  8 s window (OFF: 1.30 B). The storm is gone.
- **Correctness: intact.** repligate full PASS (snapshot ship, live
  frames, restart, SIGKILL cross-generation re-sync); workspace 205
  suites green.
- **Throughput: the needle moved only where the storm was measured.**
  sadd −15.8 % → **−7.8 % (green)**. hset (−13.4) and zadd (−15.2)
  did not move — and had never been miss-profiled; their storms may
  sit elsewhere (profiling next).

**And the correction this buys:** the serial budget model above
(+48.6 misses × ~13 cycles ≈ the whole gap) was numerology. Killing
1.7 B of those misses recovered only part of one angle: most of the
polled-line misses were latency-hidden behind other work. A budget
that reconciles on paper is *consistent with* causation, never proof
of it — **the intervention is the test** (the same lesson as v1.29's
"memcpy fraction was real but was a tax, not the bottleneck").

Post-gate follow-ups pinned the rest of the round:

- **hset / zadd / incr miss profiles are flat** (top source line
  4–6 %, no storm anywhere) — the remaining collection tax is **not a
  miss story at all**. With instructions/op *below* glibc's and misses
  near parity, the IPC gap (1.50 vs 1.67) must sit in a stall class
  these events don't see (store-side/RFO, dependency chains,
  frontend). The next decomposition round opens with a topdown stall
  breakdown, not another load-miss hunt.
- A perfgate run showed incr swinging −2.5 → −11.5 under the gate —
  refuted as noise by a 3× interleaved gated-vs-ungated A/B (mean
  +0.8 %, spread ±3 %); the gate costs incr nothing. (That perfgate's
  rerun REFUSED on the known build-independent zadd pause — the coin
  flip again.)

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
