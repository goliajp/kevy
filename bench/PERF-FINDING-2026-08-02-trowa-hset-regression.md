# T-row-a hset/incr regression — located, partially fixed, open

## Verdict state
perfgate FAIL stands. Do not ship until closed.

## Measured ladder (legacy_8sh hset shape, counter-window median-of-3, lx64)

| build | median rps | note |
|---|---|---|
| ref 4916f7eb (4.1.x baseline) | 5.18M | perfgate's reference |
| c943f7a0 (T-scalar tail) | **5.32M** | clean — the whole scalar window train costs nothing here |
| 9194a6e2 (T-row-a) | 4.41M | **the cliff: -17% in one commit** |
| 50711460 (T-row-b1) | 4.47M | b1 adds nothing |
| 39b5cbc5 (HEAD + cold_backing gate fix) | 4.63M | fix recovers ~4pp, main body remains |
| e4ae83b1 (exp: #[inline] funnels + fields-to-tail) | 4.67M | no effect — both hypotheses dead |

Stable across three interleaved perfgate runs: hset (-14..-20%) and incr
(-9..-18%). sadd/zadd/get swung with the noise band (−19..+9) — not signals.

## What perf record says (12s @499Hz, both builds, same workload)
Profiles are near-identical in shape: main dispatch block 13.8% vs 13.4%,
malloc 8.2% vs 6.7% (+1.5pp, too small for -13%), same syscall family.
**No new hot symbol.** The slowdown is not a hotspot — it smells like a
codegen/layout-level effect confined to the T-row-a change set.

## Eliminated
- cold_backing gate absence (fixed; worth ~4pp only)
- funnel #[inline] loss (exp-a: no effect)
- Store field order (exp-a: no effect)
- new hotspot (profiles same shape)

## Open hypotheses, next round (systematic decomp)
1. kevy-store's new dependency edge on kevy-seg changing codegen/link
   layout globally — test by cfg-ing the whole segrows surface out on a
   probe branch.
2. tier_read_record/tier_peek_value keyed-signature ripple breaking an
   inline chain somewhere off-funnel.
3. kevy crate's with_ready_segment three-arg change perturbing dispatch
   I-cache.
4. Binary-search T-row-a internally: restore c943f7a0's kevy-store files
   group by group on top of 9194a6e2 and measure each.

## Method notes
- redis-benchmark direct rps (P16) could NOT resolve this regression —
  the counter-window method (perfgate's) resolves it cleanly at N=3.
- perf attach trap: pgrep -f matches the bash wrapper's cmdline; use
  pgrep -x kevy.

## RESOLUTION (same day, rounds 4-5)

Root cause, nailed by symbolized profiles: `KevyMap::find_by_borrow`
(the keydir probe) ran at 3.79% self-time on both the "fixed" HEAD and
the unfixed T-row-a store against 2.61% on the clean base — per-op
that is the whole regression, and it proved **the cold_backing gate
was permanently open**. T-row-b2 moved `enable_seg_rows` to the front
of every startup path (segments load before the snapshot), and that
function set `cold_backing = true` unconditionally — every server with
a data dir paid the extra keydir probe on every read AND write funnel,
windowed or not.

The real fix (`2e3543ae`): the gate opens on cold VALUES, not on the
directory — loaded segments at enable, the first sealed segment, a
loaded snapshot stub, or the vlog tier. Empty directory = original
funnel cost.

Ladder closed (counter-window median-of-3): base 5.32M → T-row-a
4.41M → gate-fix **5.68M**. perfgate round 4: every stable signal
(hset -1.8%, incr -1.1%, get -1.9%) back inside the noise band; the
single ✗ (sadd -8.7%, floor by 0.7pp) is the known noise swinger
(-11.8✗ / +8.6✓ / -0.1✓ / -8.7✗ across rounds; candidate median 6.5M
vs ref median 6.8M = -4.5%, inside the floor). Round 5 pending as the
formal verdict.

### Crate-isolation probe that found the room
exp-c (`571dafb1`, branch perf-exp-c): clean base + T-row-a's
kevy-store only → 4.52M. The regression traveled entirely in
kevy-store, which shrank the search to the funnels and made the
symbolized-profile diff decisive.

### Lessons
- A gate is only a fix when the gate's PREDICATE is right — verify the
  fix moved the metric, not just that the code shipped (round 3 caught
  exactly this).
- Symbolized per-op normalization (pct/throughput) turns two
  same-shape profiles into a pointing finger.
