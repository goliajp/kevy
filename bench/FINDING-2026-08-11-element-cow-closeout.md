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
| 2K | **188 ms** (realistic 200-write bursts; single outlier per ~8 min) |
| 512 (~33 KB/clone) | re-verification pending (expected under the bar) |

Control discrimination (same sustained shape, rewrites OFF): worst tick
50 ms over 8 minutes — the over-bar ticks are rewrite-window-attributable,
everything else is the box's noise floor. Dataset integrity across the
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
  line, gap ≤ 100 ms): 16K FAIL (1.86 s) → 2K FAIL by one 188 ms outlier →
  512-grain re-run pending (blocked mid-arc by a host-side
  opendirectoryd/DNS outage on the dev machine; the box still holds the
  preloaded dataset for the rerun).

## Boundary that remains

Streams keep whole-value COW (documented; `XTRIM` guidance in
persistence docs). Bulk-loader mega-bursts during auto-rewrite windows
aggregate like any scattered burst — load before enabling traffic, or
expect load-phase ticks in the hundreds of ms while windows overlap the
load.
