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
