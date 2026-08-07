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
