# v2 delivers the memory half; the throughput gap survives three mechanisms

**Status:** the memory result is MEASURED and reproduced; the throughput
gap is MEASURED, size-dependent, and **its mechanism is now explicitly
unknown** — three mechanism-guided fixes changed it by nothing, which
kills the hypotheses that motivated them. Recorded per the methodology:
after two rounds that do not move the needle, the next step is
decomposition, not another round.

## The memory half: 2.40× → 1.98×

lx64, 2 M × 400 B on a 512 MB tiering budget, one shard, no AOF, each
build measured twice in alternating order:

| build | logical | resident | ratio |
|---|---:|---:|---:|
| glibc | 341.2 MB | 817.9 / 818.1 MB | **2.40×** |
| kevy-alloc v2 | 341.2 MB | 676.4 / 676.3 MB | **1.98×** |

−17 % resident on the workload the arc was built for, against v1's −3 %.
The v2 structure (occupancy bitmap in the segment header, page-granular
`MADV_DONTNEED`, lowest-first densification) does what it was designed
to do, and the kernel-level test confirms pages return from spans that
still hold live values — the case v1 could never return from.

Not the ceiling: 1.98× against a floor of ~1.0×. The remaining 335 MB
over logical is unattributed until the seven-term accounting is exported
at envelope scale (the still-pending M3 line). No claim is made about
how much of it is reachable.

## The throughput half: real, size-shaped, and unexplained

Interleaved A/B, six rounds, 50 subscribers:

| payload | ON | OFF | ratio |
|---:|---:|---:|---:|
| 16 B | 21.8 M msg/s | 23.6 M | **0.92** |
| 64 B | 18.1 M | 21.5 M | **0.84** |
| 4096 B | 1.51 M | 1.53 M | **~1.00** |

Three fixes were built against three named mechanisms, and each
mechanism is now dead by measurement:

1. **In-place `realloc`** (glibc extends, we copied): ratio unchanged
   (0.852 → 0.843). Already recorded as a skipped-gate mistake.
2. **Metadata distance** (span metadata a far cache line away): the v2
   bitmap restructured all of it — unchanged (0.826).
3. **Header touches per op** (the locality half of a thread cache): the
   hot cache removes *all* segment-line touches on the steady-state
   churn path in both directions — unchanged (0.844).

Two further facts that any real explanation must cover:

- The original counter profile stands: allocator-on executes **fewer**
  instructions and takes **fewer** page faults, at lower IPC. The work
  is the same; it stalls more.
- **Under `perf` attachment the gap shrinks to noise and once
  inverted** (ON 20.3 M vs OFF 19.9 M). A gap that a profiler's
  perturbation can erase is a timing/layout effect, not a hot symbol —
  which is consistent with three symbol-level fixes doing nothing.

The honest hypothesis space that remains is layout-shaped: *where* the
allocator places hot buffers relative to each other (page/line
colouring, prefetcher behaviour across lowest-first-packed slots, TLB
walk patterns over 4 MiB-aligned segments), not *what* it executes.
Distinguishing those needs a decomposition round of its own with
hardware counters chosen for it — not a fourth blind fix.

## Where this leaves the trade

−17 % resident memory against −8 % (16 B) to −16 % (64 B) pub/sub, fading
to zero at 4 KiB payloads. KV (M1) is still unmeasured — perfgate keeps
refusing while the box carries other load — and the C4 obligation is
gated on it. The next unit of work on the throughput side is a
decomposition, and whether to spend it is the owner's call.
