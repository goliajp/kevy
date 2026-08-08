# RFC: the fast path's flat residue — class shape, claims width, or acceptance

Status: DRAFT, awaiting owner decision.
Inputs: the 2026-08-07 decomposition chain
(`bench/PERF-DECOMP-2026-08-07-collection-tax-is-l1-misses.md`,
`bench/PERF-DECOMP-2026-08-07-hset-tax-owner-thread.md`) — every number
below is measured, none inferred.

## Where the arc leaves the collection angles

| angle | arc start | after three knives | floor |
|---|---:|---:|---|
| sadd | −15.8 % | −8.5 % | 0.92 (missed by 0.5 %) |
| hset | −13.4 % | −11.6 % | red |
| zadd | −14.4 % | −11.7 % | red |
| lpush | −6.3 % | −4.3 % | green |
| kv / incr / cluster | green | green | ✓ |

Knives landed: the inbox wake-flag gate (miss storm, +7 pp sadd), the
claims-first free (no header read on the 99.86 % path), the reciprocal
table (no division on the free path). M3 = 2.16× vs glibc 2.40× on
every leg throughout; liveness untouched; repligate PASS.

## What the residue is — and is not

The owner-thread profile after the knives is **flat**: no line above
4 %, allocator machinery totalling ~20 % of the bottleneck thread
against glibc's ~13 %. It is not stalls (topdown parity), not misses
(L1 near-parity post-gate), not the tick (55 ms per 60 M-op run), not
realloc copies (uncorrelated across verbs). The claims fast path hits
99.9 % on both sides; its per-call *instruction count* is simply
wider than a tcache push/pop: segment masking, span/word/bit
arithmetic, two accounting adds, an Option match — each cheap, all of
them summed ≈ the last ~10 %.

## The three doors

**A. Accept.** The engine's headline is KV + query + memory (all
green or winning); collections run 8–12 % behind glibc on saturated
single-key storms — a shape SME workloads rarely present (R4a: prod
incidents were 100 % read-aggregation). Merge as-is; revisit if a
consumer reports a collection-bound workload.
Cost: none. Risk: the number sits in the README's fine print.

**B. Claims get wider (engineering round, autorun-sized-plus).**
One claimed word per class means every 64 slots the path detours
through refill (retire + claim_word + discarded-page bookkeeping),
and the hot free must match `(seg, span, word)` exactly — three
compares per free. Two words per class (current + previous) or a
64-slot→128-slot claim would halve refill frequency and widen the
free-match window. Bounded design: stays lowest-first *within* the
claim, so M3's densification survives; the claims_unused accounting
already prices parked bits. Estimated by structure, not measured:
1–3 %. Needs its own battery incl. M3 4-leg.

**C. Class shape / slot addressing redesign (RFC-scale).**
The 8-stepped class ladder is why slot arithmetic needs a reciprocal
at all and why spans fragment across 79 classes. Power-of-two-friendly
classes (or per-span slot prefixes glibc-style — a free-list word
inside the freed slot) would collapse the free path to pointer
arithmetic, at a measured-in-advance memory cost: coarser classes
raise rounding waste (the `rounding` term prices it; current ladder
keeps it ≤ ~6 %), and an in-slot word resurrects the header the
design's thesis removed (the M3 test that killed the LIFO cache —
1.98× → 2.38× — bounds how far reuse-ordering may drift; an in-slot
*free-list* need not change ordering, but it does dirty a line per
free, which is the tax B6's page-return feeds on).
This door reopens the allocator's core thesis; it should not open for
a ~10 % collections-only residue unless a target workload demands it.

## Recommendation

A now, B as the next autorun round if collections stay on the
industrialization scorecard, C only with a workload in hand. sadd's
0.5 % is likely box drift — a rerun may green it without any code.

## Decision points

1. Door A / B / C (or A-then-B).
2. Whether the collection-angle floor (0.92 vs glibc-OFF) remains a
   v5 gate at all, given R4a's workload evidence — the floor predates
   the measured shape of the tax.
3. mremap for giant reallocs: throughput-irrelevant (≤1 %), but the
   zadd >3 s pause (kernel-side suspect) is a tail-latency defect on
   any door; separate small investigation, owner to rank it.

## Resolution (2026-08-09, evidence-complete)

**Door A — accept; kevy-alloc stays opt-in.** Decided under the owner's
"this is your research, you decide" delegation, on the completed Phase A
chain (bench/PERF-DECOMP-2026-08-08-zadd-sadd-alloc-tax-split.md + errata):

- The hash angle was recovered by the V4 value-inline enabler (−5.8 → −0.2
  same-box; official medians −5.5 PASS): the recoverable part is recovered.
- The sadd/zadd residue is ONE thing — the fast-path per-call cost on a
  saturated owner (owner-internal `Heap::alloc` ~23 % vs glibc ~13 %,
  ~1.7×/call). Forwarding-envelope and tree-walk hypotheses both refuted;
  B (claims widening) numerically insufficient for −10~−16; B' (envelope
  pooling) long refuted (0.5 % of allocs).
- C (class redesign) therefore prices as a many-small-knives/redesign
  program against a diffuse seat, for at most 10-16 pp on saturated
  collection angles — a poor trade against v5-industrial's timeline, and
  revisitable with a workload in hand (decision point 1's own terms).
- The alloc-ON value story (capacity/fragmentation: 2.16× vs 2.40 RSS,
  disk/stable zero-cost) stands and is delivered as an opt-in with a
  documented when-to-enable guide (docs/alloc.md), which is the honest
  industrial claims posture (charter 口径纪律).

P1's original "default ON" recommendation carried the premise "collection
angles recoverable"; the premise is now measured false for sadd/zadd, so P1
executes as opt-in. Decision point 2 (the 0.92 floor's future) and point 3
(the zadd >3 s kernel-side pause) remain open, owner-ranked.
