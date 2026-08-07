# M3: the memory half does not pay for the throughput half

**Status:** MEASURED. This is the number the arc was waiting for, and it
is negative. `kevy-alloc` costs ~16 % of pub/sub throughput and returns
~3 % of resident memory on the workload the arc was built to fix.

Recorded as a result, not a setback. Rule ⑤ of the v5 arc says a premise
that measurement kills gets changed rather than worked around.

## The measurement

lx64, `--profile release-perf`, one shard, two million 400-byte values on
a 512 MB tiering budget, append log off. 400 B is the value size at which
the original 2.24× was seen. Two binaries from the same commit.

| build | logical (`used_memory`) | resident | ratio |
|---|---:|---:|---:|
| allocator **off** (glibc) | 341.2 MB | 818.1 MB | **2.40×** |
| allocator **on**, reclaim never called | 341.2 MB | 817.1 MB | 2.39× |
| allocator **on**, reclaim on the shard tick | 341.2 MB | **790.5 MB** | **2.32×** |

Against the pub/sub A/B from the day before — allocator on 18.1 M vs off
21.5 M msg/s, ratio 0.84 across six interleaved samples with disjoint
distributions — the trade as measured is:

> **−16 % throughput for −3 % resident memory.**

## Why the middle row exists

The first allocator-on run returned pages to nobody. `Heap::reclaim` was
implemented, tested and exposed, and nothing ever called it — an
allocator has no tick of its own. Every span a shard emptied stayed
mapped *and* resident, which is glibc's failure mode reproduced by
omission rather than by design.

Wiring it to the shard tick (which already exists for tiering upkeep)
moved 2.39× to 2.32×. So the mechanism works. It just does not find much
to return.

## Why it finds so little: the reclaim unit is too coarse

glibc cannot shrink its brk arena from the middle: a freed chunk under a
live one strands its page. The design's answer was to map spans and hand
them back individually.

But **a span can only be returned when every slot in it is free**, and a
64 KiB span of the 416-byte class holds 157 slots. All 157 values have to
die before a single page comes back. glibc's unit is the 4 KiB page —
about ten values of this size. **Our reclaim granularity is roughly 16×
coarser than the thing we set out to beat.**

Demotion is LRU-ordered and allocation is time-ordered, so the two
correlate and spans do empty — but partially-emptied spans are the normal
case, and a partially-emptied span returns nothing. Different mechanism
from glibc's, same outcome.

That is the premise dying. The RFC argued the win came from being
mmap-backed rather than brk-backed. Being mmap-backed is necessary and
not sufficient: what decides reclaim is the granularity at which free
space can be handed back, and a slab allocator's natural unit is its
slab.

## What the design would have to do instead

Real allocators solve this by returning **pages inside a slab**, not
whole slabs: track free runs at page granularity and `madvise` the runs
that are wholly free, leaving the slab mapped and its live slots
untouched. jemalloc and mimalloc both do a version of this. It is a
different structure from the one in `segment.rs`, not a tuning knob on
it — span metadata would need per-page occupancy rather than a single
free list, and the free path would need to notice when a page's last slot
goes.

Two cheaper things that are *not* answers, recorded so they are not tried
as ones:

- **Smaller spans.** A 4 KiB span for this class holds nine slots and
  would reclaim far better, but the segment header indexes spans at a
  fixed size, and shrinking it multiplies metadata and the class-count
  slack the accounting already tracks. It trades the same problem for a
  different term.
- **Calling reclaim more often.** The sweep already runs every tick and
  finds nothing new; the limit is what is returnable, not how often we
  look.

## What this does not say

It does not say the allocator is bad at allocating. On microbenchmarks it
is 3-4× faster than the system allocator per operation, and the identity,
reclaim, cap and cross-thread properties all hold. It says the **reason
this crate was written** — cutting a 2.24× resident ratio — is not
delivered by the structure as built, and the throughput cost is real and
measured.

## The decision this is for

The owner asked for the memory number before deciding. It is: **3 %**,
against a 16 % throughput cost. On those numbers the arc as currently
built is a loss, and the options are to change the reclaim structure
(page-granular return inside a span), to change the target (accept that
the win is elsewhere — the microbench speed, the header-free footprint on
small values), or to stop.

Nothing has been changed on the strength of this yet.
