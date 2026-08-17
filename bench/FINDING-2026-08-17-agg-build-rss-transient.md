# The agg index build's RSS transient is ~8x its formula

Found by the 5.3 suite's full tier, which put agggate's memory clamp
back against a live server after months, in three different measurement
positions — all agreeing.

## Measurements (lx64, release, 1M rows / 10k groups, formula = 39 MiB)

| when the build runs | RSS growth |
|---|---|
| cold, pre-warmup | 305 MiB |
| after warm bursts (~30% of keys touched) | 305 MiB |
| after a full 1M-key read sweep | 305 MiB |
| after five build/drop rounds (span cache warm) | ~0 to −7 MiB |

The 305 is invariant to dataset page residency — three different
warmup regimes did not move it by a megabyte — so it is not re-faulted
dataset pages. And it vanishes entirely once the allocator's span cache
has absorbed a previous build. Together: **the build allocates ~305 MiB
of transient working memory, frees it, and the pages stay resident in
allocator spans**. The formula (39 MiB) describes the settled index,
and describes it well — the historical green of this clamp was the
churn-warmed measurement seeing exactly that settled value.

## Status

The clamp now prints both numbers every run and is advisory pending a
decision; agggate's other clamps (write tax, GROUP p99, GROUPS top-100)
stay hard. Capacity planning today should assume a build transient of
roughly 8x the settled index size on this shape.

## The open decision (owner's)

1. **Attack the transient** — the build's scratch (scan/sort buffers)
   is presumably sized by rows, not by settled entries; a streaming or
   chunked build would cap it. A decomposition arc on the build path.
2. **State it** — docs/indexes.md documents per-kind VERIFY vocab and
   memory formulas; a sentence on build-time transients would make the
   formula's scope honest without engine work.
