# FINDING 2026-08-11 — element-COW closing soak: the per-tick aggregate, named and sized

Branch `feature/element-cow-closeout`. Stage findings:
`FINDING-2026-08-11-element-cow-stage-{l,hs,z}.md`. RFC + Resolution under
`.claude/rfcs/` (local).

## What the closing soak caught that the single-write probes could not

The stage probes measured ONE write during a pinned view: 0.4-2.1 ms,
size-independent — the per-WRITE bound the RFC promised. The closing soak
(four 20M-element collections, sustained writes, `BGREWRITEAOF` every 30 s)
exposed the second axis: **a burst of hash-scattered writes first-touches
many buckets inside one reactor tick, so the per-TICK cost aggregates to
touched-shards × shard-bytes.** The total bytes unshared per window ≈ the
value size at ANY granularity — the same property fork-based page COW has —
so granularity is an amortization knob, not a total-cost knob.

Measured (box, watermark = `reactor_tick_gap_max_us`):

| granularity | scattered-burst worst tick |
|---|---:|
| 16K-entry buckets/segments | **1.86 s** (2000-field burst into a pinned 20M hash) |
| 2K | **188 ms** |
| 512 (~33 KB/clone) | **120-137 ms** (n=4; every occurrence at t≈17 s) |

Two control discriminations closed the attribution:

1. **Rewrites OFF** (same sustained shape): worst tick 50 ms over 8
   minutes — over-bar ticks only happen with windows.
2. **Strings-only writes with windows** (zero giant-collection writes =
   zero element COW): the ~127 ms tick STILL appears, same t≈17 s, and
   the correlation probe pins that instant to the `aof_rewrite_in_progress
   1→0` transition — **rewrite FINISH**, the last of the simultaneously
   forced per-shard rewrites landing. That is the S5-documented
   swap-commit-window family (`FINDING-2026-08-10-s5g-*`): the auto-rewrite
   path staggers shards precisely to avoid it, and a client-forced
   `BGREWRITEAOF` fans out to every shard at once, defeating the stagger.
   Not an element-COW seat.

Net: with windows forced every 30 s, **COW-attributable ticks stay ≤65 ms
— under the 100 ms bar**; the residual 120-137 ms once-per-cycle tick
reproduces byte-for-byte without any collection write and belongs to the
forced-simultaneous-rewrite finish path (a pre-existing, documented
family; auto-rewrite's stagger keeps it out of steady state, per the rc
soak's 98 auto-rewrites at gap ≤47 ms). Dataset integrity across the
restart-for-fresh-watermark was asserted (4 × 20M counts).

Instrument note: the gap gauge is a **monotone watermark** — a phase that
raises it masks later smaller spikes. The probe restarts the server after
the bulk preload so the sustained phase starts from a zero watermark;
preload-phase spikes (mega-batch loader pipelines, 50K scattered fields
per flush) are a bulk-load shape, reported separately from the steady-state
acceptance.

## Verdict state

- Per-write bound (RFC micro line): **met** at every stage (0.4-2.1 ms,
  size-independent to 20M, RSS transient ≈ one shard).
- Steady-state realistic-granularity soak with rewrite windows (RFC macro
  line, gap ≤ 100 ms): **PASS for the COW-attributable share** (≤65 ms at
  512-grain, by the strings-only differential above). The 120-137 ms
  once-per-cycle residual is the forced-simultaneous-rewrite finish seat —
  orthogonal to this arc, named for a future attack (candidate: extend the
  begin-gate stagger to client-forced BGREWRITEAOF fan-out).

## Boundary that remains

Streams keep whole-value COW (documented; `XTRIM` guidance in
persistence docs). Bulk-loader mega-bursts during auto-rewrite windows
aggregate like any scattered burst — load before enabling traffic, or
expect load-phase ticks in the hundreds of ms while windows overlap the
load.
