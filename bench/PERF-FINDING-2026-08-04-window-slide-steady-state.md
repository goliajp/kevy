# Window slide steady-state — the R2c criterion, measured

Probe: `crates/kevy-embedded/examples/diag_window_steady.rs`
(Manual-mode ticks, so every sample is one deterministic
`Store::tick()`; median over 100 rounds, 3 runs each; local macOS,
so SHAPES are the finding, not the absolute µs).

The criterion under test (research master plan §R2c): *steady-state
slide cost tracks the eviction rate and ignores the window's
contents; the idle tick is near zero.*

## Numbers

Axis 1 — window SIZE at fixed bucket(100)/churn(128 rows/tick);
every measured tick slides:

| span (hot rows ≈ span) | median tick µs (3 runs) |
|---|---|
| 1 000 | 19 333 / 18 956 / 19 595 |
| 10 000 | 18 745 / 18 891 / 19 788 |
| 100 000 | 21 466 / 19 902 / 18 928 |

Axis 2 — churn RATE at fixed span(10 000)/bucket(100):

| churn rows/tick | median tick µs |
|---|---|
| 128 | 17 822 / 17 947 / 18 808 |
| 256 | 18 604 / 18 752 / 18 719 |
| 512 | 19 455 / 18 428 / 19 494 |
| 1 024 | 18 924 / 19 063 / 21 483 |

Axis 2b — BUCKET at fixed span(20 000)/churn(512):

| bucket | median tick µs |
|---|---|
| 100 | 21 302 / 20 267 / 20 822 |
| 500 | 21 893 / 20 213 / 19 229 |
| 2 000 | 2 / 3 / 4 |

Axis 3 — idle tick (window full, zero churn): 0–1 µs at both
span 1 000 and 100 000.

## The scale law (what the numbers say)

1. **Size-independent: CONFIRMED.** A 100× larger hot tree
   (≈2 k → ≈200 k resident rows) moves the slide median not at all
   (~19–20 ms flat). The criterion's core claim holds.
2. **Idle near zero: CONFIRMED.** 0–1 µs regardless of window size —
   the idle-ticks discipline does its job.
3. **"Proportional to rate" needs restating.** The slide is
   at most ONE seal per tick (`WindowRt::slide` advances the boundary
   to the newest target in one seal, however many buckets behind it
   is), and one seal costs a ~19–21 ms CONSTANT dominated by
   durability I/O — `SegBuilder::finish` fsyncs the segment and the
   manifest fsyncs twice more (`kevy-seg/src/builder.rs:98`,
   `manifest.rs:120,144`). An 8× larger row batch (128 → 1024
   rows/tick) is invisible inside that constant. The honest form:

   > steady-state cost per tick ≈ **min(1, churn/bucket) × seal
   > constant**, where the seal constant is ~3 fsyncs and the
   > row-moving term (the part actually proportional to row count)
   > is buried under it.

   Axis 2b shows the `min` term live: churn 512 < bucket 2000 means
   most ticks slide nothing, and the median tick collapses to 3 µs.
4. **BUCKET is the fsync-frequency knob.** Seals happen every
   `bucket/churn` ticks; a bucket sized well above the per-tick churn
   amortizes the durability constant to near zero per tick.

## The residual observation (recorded, not attacked)

The seal's fsyncs run **inside the shard lock** (the window tick is
part of `shard_upkeep` / `Store::tick` under `lock_write`), so every
seal is a ~20 ms stall for that shard's commands. On the background
reaper cadence this is at most one stall per interval per shard, and
BUCKET sizing controls how often it happens — but it is a per-seal
p99 cliff that no BUCKET choice removes entirely. Candidate later
work (NOT undertaken here, per the two-gate discipline: no attack
without a profile showing it matters on a real workload): seal
outside the lock, or split the build (locked) from the fsync
(unlocked). Recorded for the R2 close-out.

## Criterion verdict

R2c holds where it matters — cost is independent of window contents
and the idle tick is near zero — with the rate term restated as a
seal-frequency law (`min(1, churn/bucket) × ~3 fsyncs`) rather than
a per-row proportionality. The durability constant and its
in-lock placement are the two facts a deployer needs: size BUCKET
above per-interval churn, and expect one ~20 ms shard stall per
seal.
