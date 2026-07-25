# B6 RSS overshoot under bulk ingest — glibc heap fragmentation, not a tiering bug

**Status:** ROOT-CAUSED with measurements. Tiering's logical memory bound
is CORRECT; the residual is a glibc allocator characteristic. The fix
direction touches a locked constraint (pure-Rust 0-dep) and/or the B8
acceptance metric — a decision for the owner, not an autonomous change.

## What B6 measures now (real NVMe, 5M × 4KiB SET on a 2GB budget)

Steady state after the load settles (sampled `INFO memory` + `/proc/RSS`):

| metric | value | vs 2GB budget |
|---|---|---|
| **used_memory** (tiering's logical accounting) | **1.586 GB** | under budget ✓ |
| **RSS** (process resident pages) | **2.61 GB** | +30% over |
| cold_keys / demotions_total | 4,729,976 | 14.5 GB on disk |

Tiering does exactly its job: logical memory is held at 1.586 GB and 4.73M
values (14.5 GB) are spilled to disk. Reads are all fast (D1 gates green).
The failure is only RSS vs the budget — the gap is **RSS − used_memory ≈
1.0 GB** that the OS never gets back.

## Root cause: glibc brk-heap fragmentation, confirmed reclaim-proof

The demotable values are ~4 KiB allocations. glibc's `M_MMAP_THRESHOLD`
default is 128 KB, so 4 KiB values come from the **main brk arena, not
mmap**. The brk heap can only shrink from the *top*: once demotion frees a
value whose chunk sits below a still-live chunk, that page cannot be
returned. Under B6's churn (5M allocs, ~4.73M freed by demotion, scattered
by LRU) the heap fragments and RSS sticks at its high-water mark.

Two standard reclaim levers were measured and **do not help**:

- `malloc_trim(0)`: returns 1 ("did work") but RSS unchanged — a
  standalone repro (500k × 4 KiB, free half, trim) held RSS at 1974 MB
  before and after. Interspersed live chunks block page return.
- `MALLOC_ARENA_MAX=2`: RSS identical (2611 MB) — not a multi-arena
  inflation.

So this is not reachable by tuning; it is the brk-arena's structural
behaviour for churny sub-mmap-threshold allocations. jemalloc/mimalloc
return such pages far better — but adding one violates the L2-locked
pure-Rust 0-dependency constraint.

## Scope / severity

- It is a **worst-case bulk-ingest** number: 5M × 4 KiB blindly ingested at
  ~350 MB/s into a 2 GB budget is maximum allocation churn in minimum time.
  Mixed / slower real workloads fragment far less.
- But it is real for the memory-bound promise: an operator with a 2 GB
  cgroup limit would see 2.6 GB RSS and risk an OOM-kill, even though
  tiering's logical bound is honoured.

## Decision needed (touches locked constraints — owner's call)

1. **Value-buffer pool** (0-dep-safe, real work): reuse freed value
   backing buffers across the demote/promote churn instead of returning
   them to glibc, so RSS is bounded by the pool. Custom allocation path
   for values, with correctness implications — a stone-layer change.
2. **Recalibrate B8**: treat `used_memory ≤ budget×1.05` as the logical
   bound (met: 1.586 GB) and document RSS overhead as glibc-inherent under
   heavy churn (e.g. RSS ≤ budget×1.4). Honest if we deem the logical
   bound the product contract.
3. **Relax 0-dep for the allocator** (jemalloc/mimalloc return pages):
   fixes RSS directly but breaks the L2 pure-Rust constraint.

Options 1 and 3 change the product/constraints; option 2 changes the
acceptance metric. All three are owner decisions, so this is recorded
rather than acted on. Tiering's read path and logical memory bound are
validated and green.
