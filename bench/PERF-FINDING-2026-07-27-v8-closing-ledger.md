# v8 closing ledger: from −40 % to seven-of-twelve, and the residual has a name

**Status:** the cross-shard collapse is SOLVED (root cause: mmap/munmap
churn on the process mmap_lock); the arc's remaining gap is isolated to
one mechanism class with profile numbers attached. This doc is the
consolidation of findings `mmap-lock-was-the-killer` and its follow-ups.

## The v8 verdict (full perfgate, interleaved, 3 instances)

| angle | day's start | v8 | |
|---|---:|---:|---|
| pinned_cluster_get | +0.3 % | +0.4 % | ✓ |
| pinned_cluster_set | −0.9 % | −3.5 % | ✓ |
| pinned_compat_get | **−38.5 %** | **−2.8 %** | ✓ |
| pinned_compat_set | −39.2 % | −8.0 % | ✗ by 0.035 % — noise distance |
| legacy_8sh_get | −28.4 % | **+2.1 %** | ✓ |
| legacy_8sh_set | −24.6 % | −0.5 % | ✓ |
| legacy_8sh_incr | −24.2 % | −6.3 % | ✓ |
| legacy_8sh_sadd | −18.8 % | −10.6 % | ✗ |
| legacy_8sh_hset | −21.8 % | −18.6 % | ✗ |
| legacy_8sh_lpush | −17.6 % | −12.0 % | ✗ |
| legacy_8sh_zadd | −24.3 % | −13.2 % | ✗ |
| zalg_zinterstore | −24.6 % | **+7.5 %** | ✓ win |

Memory (M3): **1.98× resident/logical vs glibc's 2.40×** — the arc's
purpose, holding. Pub/sub (M2): 0.858–0.894, unchanged by the pool.

## What fixed the collapse: three moves, one root cause

1. **Class table → 32 KiB** (−40 % → −9 %): dispatch/reply buffers just
   past 8 KiB stopped paying an mmap/munmap pair each.
2. **Hot-cache removal** (M3 2.38× → 1.98×): LIFO reuse had silently
   destroyed the densification page-return feeds on; the cache had never
   won a measurable point anywhere.
3. **Process-wide retention pool** (compat −10 % → −2.8 %, legacy get
   −15 % → +2 %): the 36 KB–300 KB buffer ladder recycles with zero
   syscalls. mmap count under legacy load: 105k/6 s → 3.4k/6 s (−97 %).
   The pool's first version was per-heap and moved nothing — the
   buffers are born on one shard and die on another, and a large
   mapping's address reveals no owner, so the pool must be shared. The
   fuzzer caught `Heap::drop` forgetting the pool within minutes
   (monotonic RSS to OOM); drop drains first now.

## The residual, isolated

hset (worst remaining angle, −18.6 %), both sides profiled:

| | allocator self time |
|---|---|
| kevy-alloc | `pop_slot` 7.6 % + `alloc` 5.3 % + `dealloc` 4.4 % = **17.3 %** |
| glibc | `malloc` 7.3 % + `cfree` 2.8 % = **10.1 %** |

Collection writes make several small allocations per op, so they
amplify the per-allocation delta that plain GET/SET absorbs. The delta
is the **original locality finding, finally isolated**: every bitmap
alloc/free touches span metadata a far cache line away, while glibc's
tcache pops from a thread-local line. Pub/sub's residual is consistent
with the same class.

**The naive fix is known to be wrong.** A heap-local free-slot cache
removes exactly those far-line touches — and measurably destroys
densification (M3 went 1.98× → 2.38× while it existed, and it never won
a point of throughput). Any next design must serve both masters:
recycle hot slots *and* keep allocation position-aware. Candidate
shapes, unvalidated: a bounded cache that only holds slots from the
current lowest span; per-word bit batching (hold a claimed word in the
heap, hand out its bits, write back once); or accepting −10~−19 % on
collection writes as the price of −17 % resident memory — an SME trade
the owner may take. Nothing below double-digit self-time gets built
(the Pre-Phase-B gate held twice today; the two times it was skipped
produced the realloc round and the rings round).

## Scoreboard discipline notes

Eight mechanism hypotheses died by measurement today (five small-path,
layout, per-heap pool, and the "large allocs are infrequent" comment
that started it all). The three that lived were all named by a counter
or a profile *before* implementation. The arc's rule — refuted premises
change, negative results are results — was exercised eleven commits'
worth.
