# diskgate red: every single-command batch pays the transaction-marker tax

Release-matrix pre-run (R4), 2026-08-09. diskgate FAIL — AOF bytes/op
178 vs the 106 baseline (+68 %), on plain `SET -d 64`.

## Anatomy (measured, hexdump of a live AOF)

Every reactor batch — including a batch of ONE command — is bracketed by
transaction marker records:

```
[8B hdr] *1 $13 \0KEVYTXNBEGIN      ≈ 32 B
[8B hdr] *3 SET key:… <64B value>   ≈ 115 B
[8B hdr] *1 $14 \0KEVYTXNCOMMIT     ≈ 33 B
                                    ≈ 180 B/op   (measured 178)
```

The V1-era baseline (106, recorded 2026-07-03, before the v2 record
envelope existed) explains 106→115; the marker pair explains the rest.
`begin_group()` writes BEGIN unconditionally (`aof_txn.rs`), and all
four reactor call sites (epoll inbox ×2, uring_io ×2) open the window
per batch regardless of batch size. A `redis-benchmark -P 1` workload —
and every non-pipelining client — is all single-command batches.

## Why the fix is a surgery, not a patch

Two responsibilities live in one window:

1. **Group fsync** (`Fsync::Always` defers to one fsync per batch) —
   correct for every batch size, costs zero bytes.
2. **Atomicity markers** (replay holds frames until COMMIT) — needed by
   `atomic()` (the embedded all-or-nothing API, 69a9e7fc) and by
   MULTI/EXEC, NOT by a pipelined read batch: Redis pipelining is
   explicitly not transactional, so bracketing it over-promises at
   +65 B/op.

The clean cut: reactor batches get a fsync-only window; the marker
window becomes the property of true atomic units (atomic(), MULTI/EXEC
if its AOF path relies on this — TO VERIFY before cutting). Options
rejected: lazy-BEGIN (ordering: the first frame is on disk before the
second arrives); callsite batch-size pre-parse (works for epoll —
first-frame consumed < buf len — but the uring path interleaves slab +
carry-over input inside `uring_recv_dispatch`, so the predicate would
have to move inside it anyway; doing the responsibility split is the
same size and honest).

Blast radius: kevy-rt (4 call sites + window API), kevy-persist
(aof_txn split), replay semantics untouched (markers keep their
meaning; they just stop appearing where no atomicity was promised).
Must re-run: crashgate (incl. any txn cells), repligate, diskgate
(expect ~115±, still above the stale 106 V1 baseline — re-record it in
the same commit with the rationale), perfgate spot check (two fewer
appends per op on the hot path can only help).

## Interim status

- diskgate stays honestly red until the surgery lands; the baseline is
  NOT widened (the tax is real, the number is doing its job).
- Slotted as release-train R2.5 (own round, fresh context) — before R5,
  since diskgate is an L4 release gate.
