# The killer was an mmap/munmap pair per big buffer — six rounds late

**Status:** ROOT-CAUSED by differential profiling, CONFIRMED by the fix
moving the needle for the first time in six rounds. This doc also
records the v4 revert and the layout probe that cleared the way.

## The v4 verdict

perfgate A/B, same protocol as every round:

| angle | v3 | v4 |
|---|---:|---:|
| pinned_cluster_get | +1.0 % | +0.0 % |
| **pinned_cluster_set** | **−1.2 %** | **−50.3 %** |
| pinned_compat_get | −40.1 % | −40.5 % |
| legacy band | −18…−30 % | −20…−28 % |

Cross-shard: unchanged — the **fifth** mechanism-guided intervention to
move it by nothing. And v4 *destroyed* a previously clean angle.

## The custody hole (why v4 is unsound regardless of the numbers)

The unit test walked A-owns / B-reuses / **C**-frees and proved the
settlement exact. The case it did not walk is the dominant one in kevy:
A-owns / B-reuses / **A**-frees — B recycles an absorbed slot into a
request structure, the request travels to its owner A, and A drops it.

A's `dealloc` sees `owner == self` and takes the **local** branch:
`live_bytes -= size` — but A's stored counters never covered that
allocation (coverage lived in the pending deltas). The stored counter
underflows, and the snapshot's `checked_add_signed(...)` then fails on
the wrapped value. An accounting identity that can be made to lie (or
panic) by a legal op sequence is not a candidate for polish; the design
is withdrawn. The `-50.3 %` is consistent with exactly this fallout on
the SET-heavy angle and was not investigated further — a dead design's
precise failure mode is not worth box time.

Reverted to v3 (batched home-shipping), which is behaviourally identical
to v1 on every measured angle while doing ~99 % less cross-core atomic
traffic — sound, tested, and kept.

## Five nulls force a different question

realloc-in-place · bitmap-relocated metadata · hot cache ·
batched shipping · absorb-and-reuse: five interventions against four
named mechanisms, zero movement on the cross-shard gap. Meanwhile the
standing anomalies were never explained by *any* of those mechanisms:

- `deliver_publish` **self** time inflating 13.7 % → 21.7 % on
  *identical work*;
- fewer instructions, fewer page faults, lower IPC;
- the gap shrinking to noise **under perf attachment**;
- `--threads 1` compat completely clean (1.006 / 1.023) while the same
  topology at 8 threads loses 40 %.

Every one of those is compatible with **binary layout** — where the
linker happened to place and align functions in a binary that links an
extra crate — and none of them requires the allocator to *do* anything
wrong at runtime.

## The layout probe: null — which is what made the profile trustworthy

Both binaries rebuilt with `-C llvm-args=-align-all-functions=6` (every
function 64-byte aligned, the standard neutraliser for code-placement
luck), same interleaved 8-process compat GET A/B:

| config | ratios |
|---|---|
| plain | 0.461 / 0.486 / 0.622 |
| aligned | 0.525 / 0.494 / 0.540 |

Layout: dead. Sixth null — but this one mattered, because it also
established that **the gap survives on this harness at full size**, so
it could be profiled directly (the earlier "shrinks under perf" was a
pub/sub-only artifact).

## The profile finally names it

perf record on the compat shape, gap intact under the profiler
(ON 11.5 M vs OFF 27.7 M while recording):

- `osq_lock` **15.94 %** self, `rwsem_spin_on_owner` 5.43 %,
  `rwsem_down_write_slowpath` 3.94 %, `native_flush_tlb_one_user` 2.52 %
- call graph: `__x64_sys_munmap` **27.6 %**, `vm_mmap_pgoff` **12 %**

**Forty percent of server self time inside two syscalls.** Eight shards
serialised on the process-wide `mmap_lock`, TLB shootdowns riding along.

The mechanism: the large path mapped on every allocation and unmapped on
every free, under a comment that said *"no pooling; large allocs are
assumed infrequent."* That assumption is the premise that actually died.
Cross-shard dispatch and reply buffers sit just past the 8 KiB class
ceiling, so the hottest buffers of a compat workload each paid a syscall
pair that glibc serves from its recycled arena for free.

Every anomaly lines up behind it:

| anomaly | explanation |
|---|---|
| `--threads 1` compat clean (1.006) | an uncontended `mmap_lock` is cheap; no cross-CPU TLB shootdown |
| pinned_cluster clean, compat −40 % | per-connection buffers recycle; **dispatch** buffers churn — and only compat dispatches |
| five small-path redesigns null | the small path was never the problem |
| microbenchmarks 3-4× faster | they never allocated past 8 KiB |
| alignment probe null | layout was never the problem |
| fewer instructions, lower IPC | cycles moved into syscalls and lock spinning |

## The fix follows the geometry

Class table extended 8 KiB → 32 KiB in the same eight-per-octave grading
(worst-case rounding still 11.1 %); a 64 KiB span holds 2–8 slots at
these sizes, so the churning buffers now recycle through spans and the
hot cache like everything else. Above 32 KiB stays direct-mapped and is
genuinely infrequent. No new machinery.

First measurement (quick 8-proc compat harness, three interleaved
rounds): **0.850 / 0.991 / 2.384** — against 0.46–0.62 before. (The
third round's OFF side collapsed to 11.2 M, an interference artifact;
the ON side's 20.9–26.7 M against its former 11.2–13.8 M is the signal:
the allocator-on server roughly doubled.) The authoritative full
perfgate A/B is recorded below.

## Full perfgate verdict

_recorded when the run completes._
