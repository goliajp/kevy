# Phase E — E2E re-bench (mac aarch64 docker plane)

After all 8 stones (P1–P8) deep-polish, the e2e bench/run.sh runs
the same valkey-benchmark workload against the kevy binary built from
the polished stones. This file diffs the post-polish numbers vs
the pre-polish baseline at `e2e-mac-aarch64.log`.

## Headline (kevy rps; higher is better)

| workload          | pre-polish | post-polish | delta    | verdict      |
|-------------------|-----------:|------------:|---------:|--------------|
| **-c1 PING_INLINE** |     5,597  |   **9,492** | **+70%** | ✅ wins (resp parser polish transmits) |
| **-c1 PING_MBULK**  |     6,124  |  **10,106** | **+65%** | ✅ wins                             |
| -c1 SET             |     6,567  |       689   | -89%     | ⚠️ regression (or single-run docker noise) |
| -c1 GET             |       616  |       637   | +3%      | ≈ same (baseline was already broken — see baseline README "GET/INCR -c1 10× internal drop") |
| -c1 INCR            |       618  |       612   | -1%      | ≈ same                              |
| -c50 PING_INLINE    |    74,397  |    68,620   | -8%      | ⚠️ slight regression                |
| -c50 PING_MBULK     |    69,145  |    73,186   | +6%      | ✅                                  |
| -c50 SET            |    66,305  |    46,879   | -29%     | ⚠️ regression                       |
| -c50 GET            |    50,219  |    35,717   | -29%     | ⚠️ regression                       |
| -c50 INCR           |    38,027  |    42,361   | +11%     | ✅                                  |

## Reading the data

### Wins

- **PING_INLINE / PING_MBULK +65-70% on -c1**: parses-and-replies one
  command per syscall round-trip. The kevy-resp polish (specialised
  Argv API, validated/copy-split multibulk parse, SWAR find_crlf)
  goes through every command in PING tests. The 65-70% lift is the
  resp-parser optimisation transmitting end-to-end as expected.

### Apparent regressions

- **-c1 SET dropped 6567 → 689**: 10× regression on a single workload
  with no obvious code-level cause (kevy-bytes Clone got faster, not
  slower; SET writes a value into the keyspace using the value-side
  byte buffer which now has a faster Clone). Two hypotheses, in
  decreasing likelihood:
  1. **Single-run noise on the mac Docker VM**. The pre-polish
     baseline already showed GET/INCR -c1 at 616 / 618 rps (10×
     lower than SET) which the baseline README flagged as "doesn't
     match the lx64 metal plane's c1 1.1–1.3× lead vs valkey". The
     mac Docker VM has a known high-noise floor on per-syscall paths
     and SET could simply have hit the same anomaly on this run.
  2. **A real cement-level regression** introduced by one of the
     stone splits (e.g., kevy-bytes Clone changes the malloc layout
     calls; if the kevy-store SET path or kevy-rt accept path picks
     up a worse code-gen choice from inlining decisions, that would
     show up here).

- **-c50 SET/GET regressed ~29%**: similar pattern; could be docker
  noise or a real -c50 regression in the runtime path.

### What to do about it

**Mac docker plane is not authoritative.** The user's
`feedback-kevy-bench-isolation` memory explicitly notes that mac
docker is starvation-biased on busy-poll servers; the
`project-kevy-roadmap-state` memory records "ALL 3 perf indicators
now lead (-c1 1.1-1.3×, -c50 1.6-2.4×, pub/sub 2.3×)" on the lx64
metal plane.

**Two follow-ups required before publishing**:

1. **Lx64 metal re-bench** (post-polish equivalent of v0.metal's
   bench-final). If lx64 metal shows the polish kept the lead on
   all 3 indicators, the mac docker regression numbers are dismissed
   as Docker VM noise and v0.publish proceeds.
2. **If lx64 metal also shows regression on SET / -c50 GET**, then
   bisect git log between commit `44002d1` (pre-polish baseline)
   and HEAD to find which stone polish introduced the gap. Most
   likely suspects: kevy-bytes Clone changes (commit `5b19262`),
   kevy-ring cached-cursor changes (commit `51b1c9b`).

The lx64 metal harness is **outside the autorun session's reach** —
it requires the user's metal box. Phase E on mac docker is recorded
here as evidence, not as the publish gate.

## Per-stone polish wins that should transmit

Stones whose perf improvements ought to show up in e2e:

| stone | change | e2e workload it should help |
|---|---|---|
| kevy-bytes | Clone heap 36 → 19 ns (35% faster) | every SET (value storage), every GET reply (bulk encode) |
| kevy-bytes | PartialEq specialised | every GET-existing-key (eq for hash lookup) |
| kevy-resp  | parses 9× faster than redis-rs (18 ns reply, 56 ns command) | every command's request parse + reply encode |
| kevy-ring  | cross-thread SPSC 52 → 4 ns (13×) | cross-shard commands (DEL/MGET/EXISTS over multiple shards), pub/sub fan-out |
| kevy-hash  | already top-tier (no change) | every hashtable lookup |
| kevy-map   | mid-table get-hit tied with hashbrown | hashtable lookup at the ~4k/shard steady state |

The +65-70% PING wins suggest the resp parser change transmitted.
The SET / -c50 regressions are the question mark and need lx64 metal
to resolve.
