# FINDING 2026-08-22 — `kevy-alloc` costs 8.5–15% more memory on the RDS workload, not less

**Status**: CLOSED. Hypothesis refuted, order excluded, direction
reproduced to within 0.1 percentage points. `kevy-alloc` should not be
enabled for this workload, and the fragmentation term is not recoverable
through it.

## The hypothesis, and why it was worth testing

The RDS comparison shows kevy holding 5,501 KB of resident memory per MB
of source CSV against PostgreSQL's 778, and — more pointedly — tiered mode
holding `used_memory` at 0.42 GiB while RSS sits at 3.03 GiB. That factor
of **7.2× between what the store accounts and what the process occupies**
is glibc: roughly eleven allocations per row, 8–16 B of chunk header on
each, and a `brk` arena that does not give pages back
(`crates/kevy-alloc/src/lib.rs:5-13`;
`bench/PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md` measured
`malloc_trim` and `MALLOC_ARENA_MAX` as no-ops).

`kevy-alloc` was written for exactly that shape: per-shard, mmap-backed,
**header-free** — it exploits Rust's sized `dealloc(ptr, Layout)` to store
no per-chunk header at all and recovers the size class by masking the
address (`kevy-alloc/src/lib.rs:17-24`) — with 79 size classes at ~11%
worst-case rounding and decay-based page return. It is gated by
`bench/allocgate.sh`, and `crates/kevy/Cargo.toml:25` has `default = []`,
so it has never been on for a released binary.

It had also never been measured on this workload. It was the one lever in
the memory column that was already built, already switchable, and entirely
unmeasured — so it went first, ahead of any design work.

## Method

`bench/allocgate-prep.sh` stages two release builds differing only in
`--features kevy-alloc`. Both were driven through `bench/pgcompare.sh
2000000 400` on lx64 — same dataset, same four durability modes, same box,
back to back.

Because the first pass ran OFF then ON, **the whole comparison was repeated
with the order reversed** (ON then OFF) as a control for anything that
drifts with time on the box.

Baseline run-to-run variance for RSS on this workload, from five
same-configuration runs earlier the same day: **under 0.5%** once the first
(outlier) run is set aside — `none` measured 5504 / 5504 / 5504 across the
stable runs. The deltas below are one to two orders of magnitude larger
than that.

## Result

RSS, KB per MB of source CSV:

| mode | OFF | ON | pass 1 (OFF→ON) | pass 2 (ON→OFF) |
|---|---:|---:|---:|---:|
| `none` | 5504 | 6336 | **+15.1%** | **+15.1%** |
| `everysec` | 5826 | 6379 | **+9.5%** | **+9.6%** |
| `always` | 5883 | 6379 | **+8.4%** | **+8.5%** |
| `tiered` | 5217 | 5875 | **+12.6%** | **+12.7%** |

Latency, p99 µs (pass 1 → pass 2 shown for ON to make the reproducibility
visible):

| shape / mode | OFF | ON | ON, reversed order |
|---|---:|---:|---:|
| page, `none` | 219 | 539 | 538 |
| page, `everysec` | 217 | 536 | 540 |
| page, `tiered` | 245 | 636 | 635 |
| idx, `none` | 348 | 464 | 473 |
| idx, `tiered` | 343 | 553 | 548 |

Tiered accounting is unchanged by the allocator — `used_memory` 1.66 GiB
either way, cold keys 556,502 against 555,377 — so this is not the store
accounting differently. It is the process occupying more pages for the same
accounted bytes.

**The direction reproduces to within 0.1 percentage points with the order
reversed.** There is no ordering effect and no run-to-run ambiguity: on
this workload the header-free allocator costs more memory and much more
tail latency than glibc.

## What this does not say

- **It says nothing about the workloads `allocgate` covers.** That gate
  exists and passes; this is a different shape (millions of small hash
  tables plus three index containers per row, ~11 allocations per row
  across a wide spread of size classes). The finding is scoped to the RDS
  workload and should not be generalised into "kevy-alloc is worse".
- **It does not explain the mechanism.** A ~11% worst-case class rounding
  and uniform 64 KiB spans (`kevy-alloc/src/class.rs:73`) are candidates,
  and so is the page-return decay never firing under a monotonically
  growing store — but no measurement here distinguishes them, and naming
  one without measuring it would be the hand-wave the perf methodology
  bans. If this is ever revisited, the next instrument is a size-class
  histogram of live allocations under this workload, not another A/B.

## Consequence

The fragmentation term in the RDS memory budget — the 7.2× between
`used_memory` and RSS — **is not recoverable by switching allocators**, at
least not by this one. It stays on the books.

That re-orders the memory work. The remaining lever is the one in
`.claude/rfcs/2026-08-22-rds-side-representation-and-paths.md` §3: a
declared table's rows carry ~62% of their footprint as the cost of not
using the declaration they were created with — a per-row hash table
answering a question the schema already answered, six heap copies of the
row key across two indexes, and an `Arc` header per row supporting COW that
is idle in steady state. That is design work, not a build flag, and it is
now the only design work in this column.
