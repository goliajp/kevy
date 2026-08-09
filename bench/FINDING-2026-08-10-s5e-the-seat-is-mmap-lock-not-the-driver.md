# S5-E: ten probes name the seat — rewrite-pipeline GB buffers vs the reactor's page faults

Instrumented decomposition of the residual tailgate reactor-gap band
(~300-700 ms, both cells, after A+B+B2), per the standing rule from the
third-seat finding: nothing merges on this surface until a profile
names it. Box NVMe, `KEVY_AOF_OFFLOAD=1` unless stated, firehose cell
(`-t set -d 65536 -P 8 -c 50`, 45-60 s windows). Diag code on
`diag/s5e-advance-timing` (throwaway branch, kept for Phase B).

## The elimination ladder (each with data)

1. **Reactor D-state blocking — NO.** 4 ms-granularity D-state sampler,
   8k polls / 32 s: 693 samples iou-wrk (offload's design victims), 109
   kevy-persist (worker fsync), **zero** shard threads.
2. **Scheduler starvation — NO.** Per-thread schedstat run_delay over
   30 s: max +94 ms cumulative (kevy-bio); shards +27-41 ms. 16 cores.
3. **Boot artifact in the gauge — NO.** Idle boot: 811 µs, stays flat.
4. **Rewrite driver paths — NO.** Bracket-timed every
   `advance_rewrite_handoff`: worst 28 ms (defer, incl. tee drop 0 ms);
   the phase split shows tick+aof = 0 ms in every slow iteration.
5. **Tier spill — NO.** Tiering defaults OFF (no budget configured).
6. **glibc trim churn — NO.** `MALLOC_TRIM_THRESHOLD_=-1` +
   `MMAP_THRESHOLD_=4M`: faults 12.9M → 15.5M, gap unchanged.

## What the phase probes showed

Per-iteration phase timing (prints ≥50 ms): slow iterations up to
439 ms live in **reap+dispatch** (up to 297 ms; single `uring_on_recv`
calls to 59 ms — 1000× the visible per-op work) and **inbound** (up to
336 ms, sometimes with comps=0). The work is ordinary dispatch; the
time is not.

Pressure probe: during 45 s of firehose the server takes
**12.5M minor faults (≈50 GB of fresh pages, ≈ every ingested byte on
a never-before-touched page)**, PSI memory `some avg10` 0.02→19.6%,
slice `full avg10` 13.6% — on a 62 GB box with free RAM and no memcg
limit. On-CPU profile: 24% libc memcpy (18.8pp = the per-op 64 KiB
`pick_value_for_set` Box clone — ingest work, not a stall),
`clear_page_erms` 4.9%, `fault_in_readable`, `try_charge_memcg`.

## The isolation matrix (45 s cells, minflt delta / gap max)

| cell | offload | rewrite | minflt | reactor gap |
|---|---|---|---|---|
| A | off | off | 1.6M | 1 593 ms (the S1 sync-append seat, as known) |
| B | on | off | 9.0M | **20.7 ms — the bar, met** |
| C | on | on | 11.4M | 1 889 ms |

**B proves the fault volume alone does not produce the gap** (9M faults,
20 ms gap). B→C's only delta is the rewrite pipeline's buffer
lifecycle: the worker serializes a multi-GB image Vec (doubling
growth), appends+fsyncs GB tee generations, and frees them — GB-scale
mmap/mremap/munmap churn on the worker thread.

## Named hypothesis for Phase B (one confirming probe still owed)

**mmap_lock (the process-wide VMA lock): the worker's GB
allocate/free operations hold it for write (a multi-GB munmap zaps
page tables for hundreds of ms); the reactor's ~270k faults/s each
take it for read and pile up behind the writer.** One theory covers
every observation: B green with 9M faults (no worker churn → faults
uncontended), C's episodes spread across dispatch AND inbound (any
faulting allocation), single recv calls at 59 ms with µs of visible
work, zero D-state (the wait is a killable/spin lock path), S5-D's
chunked tee making it WORSE (more mmap events on both sides), and this
project's own precedent (the v8 memory-experiment round already
convicted mmap_lock once).

Phase B candidates (next round, in likely-order):
- Stream the rewrite image serialization straight to the tmp file
  (bounded buffer, no GB `plan.body` Vec) — removes the largest single
  mmap/munmap pair.
- Pool/reuse tee buffers across generations (never munmap; shrink via
  `MADV_DONTNEED` off the reactor if at all) — kevy-alloc's span
  discipline, applied to the two big rewrite buffers even with the
  global allocator off.
- Confirming probe first: `perf lock` / mmap_lock tracepoints during
  cell C, or an ablation that pools ONLY the image buffer.

## Also in this round

- spop_storm CI flake #3 (new signature: replica runtime thread
  EXITED): box repro exhausted honestly (50 plain + 20 instrumented
  full-suite runs, zero repro — it wants the slow shared runner). The
  trap now self-diagnoses: the harness recorded-exit-reason fix + the
  covgate 40→200-line truncation fix are merged (`9af9718e`); the next
  occurrence prints the Err/panic verbatim. Log archived in
  bench/flake-archive/.
