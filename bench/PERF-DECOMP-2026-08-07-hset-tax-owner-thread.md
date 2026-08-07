# The hset tax, un-diluted: the bottleneck thread pays the allocator's fast path

Continuation of `PERF-DECOMP-2026-08-07-collection-tax-is-l1-misses.md`
(whose own intervention test had left hset/zadd unexplained). Four
measurements, each killing the previous layer's hypothesis:

## 1. Topdown says "no stall story"

TMA L1/L2 on the hset storm, ON vs OFF, near-identical:
retiring 48.8/50.0, backend 29.3/27.9, memory-bound 5.0/4.9,
core-bound 23.6/23.4, equal instruction totals per window. The
remaining tax is not a pipeline-efficiency story **at the process
level** — which turned out to be the diluted view (below).

## 2. The instruction-shape shift: more iterations, smaller batches

Instruction-event sampling showed ON moving work from batch processing
(`dispatch_batch` −33 % absolute) into per-iteration overhead
(`run_uring` +22 %, kernel syscall+audit path up across the board).
Counted directly: **OFF makes 3.85–3.88 `io_uring_enter` per op; ON
makes 4.47–5.15** (+15–34 %).

## 3. Per-thread topology: one saturated owner, seven spinners

The hset angle writes one hot key. Per-thread syscall counts: the
owner shard makes **~2.6 k** enters per 8 s (it never blocks — it is
the throughput ceiling); the other seven make ~20 M each, spinning on
forward/reply. Every earlier whole-process profile diluted the owner
1:8 under spinner noise — including this arc's "allocator self-time is
par" reading. The forwarders' extra enters (move 2) are a *symptom*:
a slower owner gives them more empty iterations per op.

## 4. The owner thread alone: the fast path is the tax

Cycles, owner tid only:

| allocator symbols | OFF (glibc) | ON (kevy-alloc) |
|---|---:|---:|
| malloc / Heap::alloc (+pop_slot) | 5.0 % | 10.5 + 3.6 % |
| cfree / Heap::dealloc | 8.4 % | 9.2 % |
| **total** | **13.4 %** | **23.3 %** |

Everything else matches within noise (`find_by_borrow` 17.2/15.8,
insert 6.5/6.0, hset_one, make_mut …). **On the thread that sets the
throughput, kevy-alloc's small-path per-call cost is ~1.7× glibc's,
+10 pp of the bottleneck thread ≈ the −12 % hset tax.** No storms, no
stalls, no ticks — the fast path simply does more per call than a
tcache push/pop.

## The attack face (next round, with its probe named first)

- **dealloc reorder**: `dealloc_small` reads the segment header
  (owner check) before trying the claims-hit recycle, which needs no
  header at all — claims only ever cover own spans. Claims-first
  saves the header touch on every recycled free.
- **claims hit-rate probe**: `Heap::alloc` at 2× malloc suggests
  refill/slow-path runs more than the claims design intends; count
  pop_claimed hits vs refills vs slow-path entries under hset before
  touching anything (the removed-LIFO-cache lesson bounds the design
  space: whatever widens reuse must stay lowest-first enough to keep
  M3's densification).
- zadd presumably shares the shape (same verbs family, same tax
  class) — verify with one owner-thread profile before assuming.

## Lessons banked

- A saturated-single-shard angle measures ONE thread; whole-process
  profiles of it are 1:8 diluted. Profile the bottleneck thread.
- Two benchmarks overlapped on the box mid-round (my isolation
  violation — one measurement at a time; the contaminated rounds were
  discarded and the wait-for-quiet guard now lives in the scripts).

---

## Round follow-through (same day): rates, the reorder, and what's left

- **Branch-rate probe**: claims hit 99.88 % on alloc, 99.86 % recycle
  on free; slow path and span scans are trace-level. The fast path's
  *per-call cost* is the whole face. (The probe's own global atomic
  counters contaminated one owner profile — 8 threads bouncing the
  counter lines; its ratios stand, its profile was discarded.)
- **Claims-first free landed** (`42198079`): the recycle path no
  longer reads the segment header (the claims match already proves
  ownership). hset owner A/B, 3× interleaved: +0.8/+2.4/+4.4 %,
  mean **+2.5 %** vs the unreordered build. The growth pushed
  `heap.rs` past 500 LOC; the free side now lives in `heap_free.rs`.
- **Post-reorder owner srcline**: the profile is now *flat* — top
  line 3.94 % is `segment.rs:141`, the `off / size_of(class)`
  division `slot_index_of` runs on every free (and inside claims
  matching). A per-class magic-reciprocal table (mimalloc's move) is
  the one remaining named knife, worth ~2–4 %. `class::index_of` is
  already a lookup table; its 3.92 % line is entry overhead, not a
  knife.
- **Honest residue**: after the gate (+8 pp sadd) and the reorder
  (+2.5 % hset), the ON-vs-glibc collection gap is a **broad ~10 %
  spread across the whole alloc/free machinery** with no dominant
  seat. Per-call the machinery is simply wider than a tcache
  push/pop; closing the rest is either many small knives (reciprocal
  division first) or a class-shape/claims-width redesign — a design
  decision with M3 interplay, not an autorun-sized change.

---

## The reciprocal knife, and the official two-knife table

`slot_index_of`'s division became a per-class `ceil(2^32/size)`
multiply-shift (`0ce0397d`) — exact by Granlund–Montgomery on this
domain and exhaustively tested over all 79 classes × 64 Ki offsets.
hset owner A/B, 6 interleaved rounds (one collapsed-baseline outlier
excluded): **+3.9 % mean, with visibly tighter spread than baseline**.

perfgate on the two-knife tip (gate + claims-first + reciprocal), full
run, box drift ≤ ±4.3 %:

| angle | arc start | now | floor |
|---|---:|---:|---|
| sadd | −15.8 | **−8.5** | misses green by 0.5 % |
| hset | −13.4 | −11.6 | red |
| zadd | −14.4 | −11.7 | red |
| lpush | −6.3 | **−4.3 green** | ✓ |
| incr / get / set | green | green (−5.5/−5.4/−7.4) | ✓ |
| zinterstore | +3.1 | +18.9 | ✓ (tiny-n angle, treat softly) |

The ad-hoc knife means (+2.5 %, +3.9 %) overpromised what the ratchet
banked (−13.4 → −11.6 on hset): band-edge means with round scatter
inflate; the official interleaved table is the ledger. Every
collection angle moved double digits → single digits across the arc;
none but lpush is green yet. What remains is the flat ~10 % machinery
spread — the class-shape / claims-width design question (M3 interplay)
already on the owner's table, plus sadd needing half a point.

## A second official run, and the band it exposes

Same tip, same protocol, next run: sadd −9.6, hset −9.4, zadd −15.7,
set −10.5, incr −11.5 — against the first run's −8.5/−11.6/−11.7/
−7.4/−5.5, with the reference legs themselves +3–5 % faster this
time. **The per-run angle band is ±3–6 pp**; "sadd is 0.5 % from
green" was a single-run reading, and no single perfgate run can green
or red an angle inside that band. Declaring any collection angle
green now requires median-of-N runs (the methodology's own
bench-infra clause). The two-run quote is the honest scoreboard:
sadd −8.5/−9.6 · hset −11.6/−9.4 · zadd −11.7/−15.7 · lpush
−4.3/−6.1 — all still double-digit improved from the arc's start,
none provably green but lpush.

## The median ledger (perfgate-median, N=3, zero REFUSED)

With the drain-budget fix killing the zadd coin flip, the first
median-of-3 gate run closes the book on single-run dancing:
get −0.8 · set −4.4 · lpush −5.5 · cluster/compat green ·
zinterstore +9.0 — and **sadd −10.6 · hset −9.7 · zadd −13.9 ·
incr −10.4 below floor on medians**. That is the definitive current
distance to the alloc-off reference; the single-run highs (sadd −7.8)
and lows (zadd −15.7) were both band edges. The residue RFC's doors
now have their exact number to price against.

## Generalization check: zadd yes, incr only partly

Owner-thread profiles on the final tip, ON vs OFF per verb:

| verb | OFF allocator | ON allocator | delta | median tax |
|---|---:|---:|---:|---:|
| hset | 13.4 % | 23.3 % | +10 pp | −9.7 |
| zadd | 13.3 % | 23.5 % | +10.2 pp | −13.9 |
| incr | 6.7 % | 9.7 % | **+3 pp** | −10.4 |

zadd replicates hset exactly — the fast-path story covers the
collection writes. **incr does not fit**: its allocations are only the
forward machinery's envelopes (argv husks are already pooled on the
batch path), the allocator delta explains ~3 pp of its −10.4 median,
and the rest is spread thin across every symbol. With incr's
historical band the widest of all angles (−2.5 … −11.5), its n=3
median deserves a wider-N pass before any mechanism hunt. For the
residue RFC this adjusts door B: claims widening is weakly motivated
at 99.9 % hit rates; if a forward-path knife exists it is envelope
pooling for the *single* Request/Response arms, not claim shape.

## incr, characterized and retired from the hunt

Eight interleaved ON/OFF rounds: ratios 0.878 / 0.908 / 0.934 / 0.943
/ 1.028 / 1.029 / 1.111 / 1.317 — **median ≈ 0.99, band 0.88–1.32**.
No demonstrable allocator tax on incr; the perfgate −10.4 median was
this band meeting the (itself drifting) reference base at n=3. Two
rounds also caught both binaries dropping 40 % in absolute rate
simultaneously — shared-box interference the interleave can absorb in
the ratio but not in the band. incr leaves the mechanism-hunt list as
a noise-dominated angle; any future verdict on it needs a quieter box
or N well past 8. The collection-write trio (sadd/hset/zadd, all
mechanism-confirmed) is the real remaining distance.
