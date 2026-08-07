# Header-free is not free: it trades bytes for locality

**Status:** ROOT-CAUSED with measurements. The premise that made
`kevy-alloc` attractive — no per-allocation headers — has a cost the RFC
did not account for, and that cost is currently larger than the benefit
on the pub/sub path. **This overturns a design claim rather than
revealing a bug**, so it is recorded before anything is changed.

## What was measured

lx64, `--profile release-perf`, one shard, 50 subscribers, 64 B payloads.
Two binaries from the same commit, one built with `--features kevy-alloc`.
Six interleaved rounds, order flipped each round (`bench/allocgate.sh` M2):

```
allocator OFF  21.80  21.48  21.88  21.65  21.51  ...  M msg/s
allocator ON   18.78  18.13  16.59  18.26  18.36  18.11 M msg/s
```

The distributions do not overlap. **Ratio 0.84 against a floor of 0.92**
— the KV/pubsub non-regression obligation fails.

## The counters say we are not doing more work

`perf stat` over a six-second window of the same load:

| | allocator ON | allocator OFF |
|---|---:|---:|
| instructions | **28.24 B** | 29.98 B |
| cycles | **21.82 B** | 19.55 B |
| **IPC** | **1.29** | **1.53** |
| cache-references | **2.169 B** | 1.855 B |
| cache-misses | 290 M (13.4 %) | 286 M (15.4 %) |
| dTLB-load-misses | 2.86 M | 2.74 M |
| page-faults | **61** | 1 415 |

We execute **6 % fewer instructions** and take **1 415 → 61 page faults**
(large pre-mapped segments doing their job), and still spend **12 % more
cycles**. IPC falls from 1.53 to 1.29. Absolute cache misses are level
while **cache references rise 17 %**.

That shape is not "doing more work". It is the same work, stalling.

## Root cause: the metadata moved away from the data

glibc stores a size header immediately before every chunk. The RFC
treated that as pure overhead we were clever to avoid — the address can
answer the same question, so no header is needed.

The address does answer. **Answering costs a cache line.**

A chunk header shares a cache line with the start of its own payload, so
a `free` that touches it is nearly free once the payload is warm. Our
span metadata lives in the segment header, 64 KiB to 4 MiB away from the
slot it describes. Every `alloc` reads `spans[ix].free_head` and writes
it back; every `free` does the same. Both touch a line that the payload
access will never bring in.

Two allocator symbols confirm the shape rather than the size:
`Heap::dealloc` at 2.23 % and `Heap::pop_slot` at 1.93 % of self time —
small, because the instructions are few. The cost is not in them; it is
in the caller stalling. `deliver_publish` self time goes 13.74 % → 21.68 %
between the two builds while doing identical work.

So the trade is real and it was made blind: we save 8–16 bytes per
allocation and pay an extra cache line touch per operation.

## What this overturns

Two claims, both written down before they were measured:

1. **"This allocator serves sized deallocation only, so it stores no
   headers at all"** was presented as a straight win. It is a trade, and
   on a small-object, high-rate path the locality side dominates.

2. **"There is no thread cache in front of this heap"** — argued on the
   grounds that a thread cache exists to avoid a shared heap, and ours is
   already thread-local, so a cache would sit in front of a cache. That
   reasoning is now visibly incomplete. **A thread cache is also a
   locality device**: it keeps the hot free list in thread-local memory
   that stays warm, instead of in a segment header the payload never
   pulls in. mimalloc caches per-class free-list heads in the heap for
   exactly this reason, and reading its structure as "a lock-avoidance
   device" was reading half of it.

The measurement did not find a bug in the implementation. It found that
one of the two things this design is *for* costs more than it saves in
this workload.

## A failed round, and the gate it skipped

Before the counters were taken, one targeted change was made and it did
nothing: implementing `realloc` so that growth inside a size class stays
in place, rather than always allocating, copying and freeing. Ratio went
0.852 → 0.843 — unchanged.

The profile had shown libc's `realloc` at **2.32 %** of self time, and
the project's own perf methodology has a gate for precisely this:

> **Pre-Phase-B gate:** before implementing, perf-record must show the
> attack target at **double-digit percentage points** of self time.
> Below that the µs estimate is hand-waving.

2.32 % is not double digits. The gate would have refused the change, and
it was not applied. The `realloc` implementation is worth keeping on its
own merits — an allocator without one copies on every buffer growth —
but it was not an answer to this question and should not have been tried
as one.

## What follows

Not decided here. The honest options, in the order the model suggests
them:

- **Cache the current span's free-list head in the heap**, writing back
  to the segment header only when the span changes. That is the locality
  half of a thread cache without the lock-avoidance half we do not need,
  and it is where mimalloc puts it.
- **Put the metadata back beside the data** for small classes — which is
  a header, and would mean conceding the point outright.
- **Accept the loss on this shape** if the memory win (the reason the
  arc exists) proves large enough to pay for it. That is a decision for
  the owner, not a fix, and it needs the M3 memory numbers first — which
  are not in yet.

The third is listed because it may be right. This arc exists to cut a
2.24× resident-memory ratio, and a workload trading throughput for
memory is the trade an SME box is short of. But nothing has been measured
on that side yet, so nobody can weigh it — and the KV lines (M1) have not
been measured at all, because `perfgate` refuses to run while the box has
other work on it.
