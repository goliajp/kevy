# per-word bit batching: the residual's first mechanism that lives

**Verdict: ALIVE.** The v8 ledger's residual (span-metadata far-line
touch on every bitmap alloc/free, 17.3% of collection-write self time)
gets its first mechanism that moves the needle without paying the
memory result back. Commit `21401eaa` on `feature/v5-memory`.

## The mechanism

`SpanMeta::claim_word` marks every free bit of the LOWEST holed word
live in one header round-trip; the heap hands bits out and recycles
same-word frees from its own copy (two register ops, no header
access); `retire_word` returns unused bits with the hint walked back.
Position-awareness coarsens bit → word instead of being bypassed —
the exact axis on which the dead heap-local LIFO cache failed.

## 12-angle A/B (perfgate interleaved, ON vs OFF same commit, lx64)

| angle | v8 | per-word | Δ | |
|---|---:|---:|---:|---|
| pinned_cluster_get | +0.4 | +0.1 | ~ | ✓ |
| pinned_cluster_set | −3.5 | −3.1 | ~ | ✓ |
| pinned_compat_get | −2.8 | +2.5 | +5.3 | ✓ |
| pinned_compat_set | −8.0 ✗ | −2.6 | **+5.4** | **turned green** |
| legacy_8sh_get | +2.1 | −3.0 | −5.1 | ✓ (drift band) |
| legacy_8sh_set | −0.5 | −5.0 | −4.5 | ✓ (drift band) |
| legacy_8sh_incr | −6.3 | −9.8 ✗ | −3.5 | suspect drift: incr barely allocates |
| legacy_8sh_sadd | −10.6 ✗ | −11.3 ✗ | ~ | flat |
| **legacy_8sh_hset** | **−18.6 ✗** | **−12.5 ✗** | **+6.1** | the primary target, converging |
| legacy_8sh_lpush | −12.0 ✗ | −5.2 | **+6.8** | **turned green** |
| legacy_8sh_zadd | −13.2 ✗ | −13.0 ✗ | ~ | flat |
| zalg_zinterstore | +7.5 | −3.4 | −10.9 | 3.4k-ops angle, noise-dominated (box drift shows +7.5% on same code) |

Box-drift context recorded in the same run: the reference binary vs
its own recorded baseline moved ±3–6% per angle (compat_get −6.3% on
identical code), so single-run deltas inside that band are not
verdicts. The three big collection-write convergences (+5.4/+6.1/+6.8)
sit above it and are the shapes the mechanism targets — short-lived
small allocations whose free lands back in the claimed word. sadd/zadd
staying flat is consistent: their nodes live long, frees miss the
in-flight word and still pay the far line.

## M3 residency (the hard gate): held exactly

2M × 400B on a 512MB tier budget, demote churn, one shard,
`used_memory` identical (357,825,200) on both:

| | RSS | resident/logical |
|---|---:|---:|
| OFF (glibc) | 837,772 kB | **2.40×** (v8: 2.40×) |
| ON (per-word) | 692,388 kB | **1.98×** (v8: 1.98×) |

The claimed-bit page-pin lag (≤64 slots/class) is invisible at this
scale — densification's page-granular reclaim is untouched. This is
the property the dead LIFO cache destroyed (1.98× → 2.38×); per-word
keeps it to the digit.

## Also observed, honestly

- **pubsub A/B 0.847–0.848 vs v8's 0.84** — flat. The pubsub residual
  is a different class (frees cross shards through the outbound ring,
  never landing in a claimed word); per-word neither helps nor hurts.
- **One perfgate run REFUSED on legacy_8sh_zadd**: "INFO stats
  unreadable — is a shard wedged?". Not reproducible: 60M-op zadd in
  isolation completes in 8s with flat RSS and answers PONG; the full
  gate rerun measured the angle normally. Recorded as a flake with an
  open eye — if it recurs, it gets the instrument-before-concluding
  treatment.
- **allocgate-mem's own runner exits 1** after the load phase (its
  sampler/read sequencing, not the engine: the same shape probed by
  hand is alive, readable and stable through the drain window on both
  binaries). The gate script owes a fix; the M3 numbers above are from
  the hand probe with the identical workload arguments.

## What remains red, and where the next round points

hset −12.5 / sadd −11.3 / zadd −13.0 against the 0.92 floor. The
converged angles say the claimed-word shape is right for hash-node
churn; the flat angles say sorted/set node lifetimes escape it. Next
candidates, in ledger order: profile whether the remaining hset tax
is still allocator self-time or has moved (Pre-Phase-B gate before
anything is built), and whether sadd/zadd frees can be made
word-local by allocation-site grouping — or whether their tax belongs
to the value-representation unit, not the allocator.
