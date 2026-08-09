# AOF offload S1: the client bars go green, and the gap moves to the rewrite

Release-train R2, slice S1 (2026-08-09). `KEVY_AOF_OFFLOAD=1`,
off by default; branch `feature/r2-aof-offload`.

## What S1 delivered (box-measured, real disk)

Append writes ride the shard's ring as positioned write SQEs; the
everysec fsync is a DATASYNC SQE with the window clock CQE-side.
tailgate on the NVMe (TMPDIR=captmp — see the tmpfs trap below):

| cell | bar | pre-offload baseline | S1 |
|---|---|---|---|
| mixed PING p99.9 | ≤100 ms | 100.8 ms (edge) | **10.5 ms ✓** |
| firehose PING p99.9 | ≤100 ms | 280-460 ms | **19.3 ms ✓** |
| mixed reactor gap | ≤100 ms | ~6 s | 818 ms ✗ |
| firehose reactor gap | ≤100 ms | 1.3-9.5 s | 9.5 s ✗ |

Correctness on the box: graceful-restart replay parity (31 594 =
31 594), kill -9 mid-storm boots clean (45 675 keys, zero corrupt-frame
WARNs), crashgate PASS in BOTH modes, repligate PASS.

## The second seat, convicted by ablation

With auto-rewrite disabled, the firehose reactor gap collapses
**9 514 ms → 195 ms (48×)**. The append `write(2)` has left the reactor;
what remains parked on it is the **rewrite's structural writes**:

- `finish_concurrent_rewrite` appends the tee (the diff of every write
  that landed during the off-thread spill — GB-scale under a firehose)
  synchronously, then `sync_all()`s the ENTIRE new image (tens of GB of
  dirty pages the worker just wrote) — on the reactor thread.
- `begin`'s `flush_queued` is covered: the offload gate now requires an
  empty queue as well as an empty in-flight set, so begin never has
  bytes to flush.

The residual 195 ms (rewrite off) is a third, smaller seat — unnamed,
next decomposition's problem; it is already within 2× of the bar.

## Why the finish fix is its own slice (S4), not a patch

Moving the tee append + fsync off the reactor changes WHERE the crash
truth lives across the swap: today the old file (which already holds
the tee'd writes via the normal append path) is truth until `rename`,
and the new image + tee is truth after — continuity holds because the
tee hits the tmp file BEFORE the rename. Handing the tee to the worker
means the reactor keeps writing new appends against a file the worker
is about to rename: it needs a second-generation tee for the handoff
window, worker→reactor completion signalling for the reopen/re-anchor,
and a defined loss window for a crash between rename and the tee's
landing. That is a two-phase rewrite protocol (Redis-grade complexity),
slotted as S4 with this finding as its Phase A.

## Fixed on the way (all in this branch)

- `append_marker` wrote the EXEC transaction markers straight to the
  file — in queued mode that interleaves with in-flight positioned
  writes and corrupts the log. Markers now ride the queue; they also
  join the rewrite tee (they were silently dropped from post-rewrite
  logs — a pre-existing bug: a rewrite during an EXEC un-bracketed it)
  and finally count toward `size_bytes` (they never did).
- The tmpfs trap, again (compressgate's lesson): a non-login shell
  loses `TMPDIR=captmp` and tailgate lands on the 32 GB tmpfs, which
  the offloaded firehose fills in ~57 s — ENOSPC, then an abort. Real
  measurement needs the real disk; the ENOSPC abort itself is a
  robustness item worth a look (server should degrade, not die).

## State

- S1 mergeable: off-by-default flag, default mode byte-identical
  (crash/repli green), EXEC-marker fix is live in both modes.
- R2 remaining: S2 (`always` CQE-gated replies), S3 (epoll writer
  thread), S4 (rewrite two-phase, the named seat), S5 (tailgate green —
  needs S4 plus the 195 ms residual).

## S4 postscript (same day): the rewrite seat collapsed as predicted

Two-phase handoff landed (`feature/r2-s4-rewrite-handoff`): the driver
hands large tee generations to the persist worker (append+fsync
off-thread) while writes keep teeing into a fresh generation; the
reactor pays only a bounded (≤4 MiB tee) synchronous final swap,
handoffs capped at 4. Real-disk tailgate, offload on, rewrite ON:

| cell | gap @ S1 | gap @ S4 | PING p99.9 |
|---|---|---|---|
| firehose | 9 514 ms | **314 ms (30×)** | 61.7 ms ✓ |
| mixed | 818 ms | **581 ms** | 9.8 ms ✓ |

The rewrite-ON firehose gap now sits at the same order as S1's
rewrite-OFF ablation (195 ms) — the seat this slice was named for is
gone. The residual (314/581 ms vs the 100 ms bar) is the third seat:
unnamed, needs its own decomposition before S5 can close.

Review bonus: an S1-latent uring exit-order bug (shutdown_drain — which
can rename+reopen the AOF and append-flush the queue — ran BEFORE the
in-flight positioned writes drained; a straggler CQE could then land in
a reused fd number). Exit now drains the ring first.
