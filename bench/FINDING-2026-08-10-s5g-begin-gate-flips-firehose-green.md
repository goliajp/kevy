# S5-G arc: the begin-gate flips the firehose green; mixed's 340 ms constant survives seven probes

Continuation of the S5-E/F finding. Branch `feature/s5g2-begin-gate`
(NOT merged — see the mixed regression below). All numbers median-of-3
tailgate cells on the box NVMe, offload on.

## The file-tee (RFC S5-G) was built, measured, and REJECTED

Full implementation (staging + `<aof>.tee` ring writes + worker
file-to-file folds + watermark convergence; byte-level round-trip test
caught a write-only-fd EBADF): firehose PING p99.9 median regressed
65.6 → 141.6 ms — moving the tee to disk trades reclaim pressure for
~3× write traffic on the already-saturated device. Both substrates
prove one theorem: **under saturating ingest the rewrite ATTEMPT is
the disturbance.** Code preserved in branch history
(`feature/s5g-tee-to-file`, deleted; commits reachable via the finding
trail), design in `.claude/plans/2026-08-10-s5g-tee-to-file-rfc.md`.

## What flipped the firehose green (stable across 4 verdict rounds)

`feature/s5g2-begin-gate`, on the merged F-chain base:

1. **Begin-gate**: at the auto-rewrite trigger, sample the append
   rate; above TEE_DEFER_CAP/2 per second, skip the attempt.
2. **Hysteresis** (~2 s sustained calm required): the mixed cell's
   rate straddles any threshold (redis-benchmark's read phases are
   genuine zero-append calms), and a momentary lull otherwise admits
   a giant postponed attempt (measured: a consistent 1.12 s stall).
3. **Unified worker Cleanup job**: the F-chain's DropBufs+Remove pair
   raced the serial worker — the second submit silently fell back to
   an inline GB unlink on the reactor. One job carries paths + bufs.
4. **Swap graveyard hardlink**: a successful swap's rename(2) dropped
   the multi-GB pre-swap log's LAST link — extent-freeing inside the
   syscall on the reactor. Queued mode now hardlinks to `.trashN`
   first; the worker unlinks it.
5. **The overrun valve runs OUTSIDE the structural gate**: a
   single-key LPUSH storm (redis-benchmark's fixed `mylist`) keeps
   one shard's queue non-empty forever, the gate never opens,
   tick_persist never runs, and the tee overrun check inside it never
   fired — a GB Vec tee grew with zero defers logged (21.9 GB shard-0
   log vs 0.5 GB peers). The check is memory-only + worker-shipped
   unlinks, so it now runs even with chunks in flight.

**Firehose: gap 23-85 ms, PING p99.9 10.5-13.6 ms — both bars, four
consecutive rounds.** The V3 train's named target cell.

## The mixed cell's ~340 ms constant — portrait of an unnamed seat

With the gate admitting attempts, mixed shows a uniform ~313-349 ms
gap / ~430-450 ms client max, EVERY run (vs develop's 43 ms median
with the same event appearing in ~1/3 of runs — the gate made a
pre-existing occasional event deterministic, likely by letting the
admitted attempt run long instead of deferring early). Eliminated
with instrumentation, each with numbers:

- collect_snapshot: 15-27 ms at 250-500k entries; begin+tee 0 ms.
- rename's implicit unlink: graveyard hardlink changed nothing.
- tee overrun: valve ungated, still ~340 ms (and defers now fire).
- COW clone of the shared giant list (`Arc::make_mut` on mylist):
  zero hits ≥10 ms. Same for the giant set.

Facts the next probe must fit: needs rewrite machinery ON (rewrite-off
= 45-50 ms); one event per run; magnitude invariant across five
builds; coincides with the single-key LPUSH/SADD phases (shard 0 holds
a 21.9 GB log and a tens-of-GB in-flight rewrite image). Candidate
probes: port the per-iteration phase probes to this branch; instrument
the hash/other `make_mut` sites and the actual LPUSH dispatch path;
bracket the worker-completion commit path.

## State

- Branch `feature/s5g2-begin-gate` on origin: 5 real commits + 3
  throwaway diag commits (strip before any merge).
- NOT merged: mixed median 347 ms vs develop 43 ms is a regression at
  the median even though the event itself pre-exists on develop.
- Next round (S5-H): name the 340 ms event, then re-verdict; the
  firehose flip merges together with the mixed fix.
