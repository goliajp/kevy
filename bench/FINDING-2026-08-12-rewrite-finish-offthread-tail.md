# FINDING 2026-08-12 — the forced-rewrite finish seat: named, knifed, closed

Branch `feature/rewrite-finish-offthread`. Upstream:
`FINDING-2026-08-11-element-cow-closeout.md` (the 120-137 ms
once-per-cycle tick that survived element-COW and refuted the stagger
attack).

## The decomposition trail

1. **Arm-level timers** (diag branch) put the whole cost inside ONE
   completion arm: `tee-appended took 288781us` — always the same shard
   (the deterministic owner of a giant key), everything else <9 ms.
2. **A wrong knife, honestly kept**: the arm's buffer recycling could
   drop GB-capacity Vecs inline on the reactor (three leaks past the S5
   "big frees go to the worker" discipline — `stash_tee_spare`'s slot
   loser, the drained empty generation's scope-drop, and the finish
   path's `rewrite_tee = None`). Fixed — and the 283 ms did not move:
   after the restart-for-fresh-watermark, every buffer is KB-scale. The
   fix stands as hygiene for the GB-scale case; it was not this seat.
3. **Step-split timers** named it: `syncswap` lines on EVERY shard —
   the rewrites were all taking `finish_rewrite_swap`, the reactor-side
   SYNCHRONOUS append+fsync+rename+reopen. **Trickle ingest never
   drains the tee empty** (every handoff round leaves a few hundred
   bytes), so the MAX_HANDOFFS backstop fired every time and the S5
   off-thread SwapImage — gated on an EMPTY tee — never engaged.
   3-9 ms typically; ~300 ms when the rename/fsync landed in a loaded
   jbd2 commit window. That is the whole story of the "finish seat".

## The knife

The terminal branch (tee empty OR converged-small after MAX_HANDOFFS)
now hands the residual ≤SMALL_TEE generation to the worker as the swap
job's **tail**: append, fsync, hardlink, rename — all off-thread; the
reactor holds its append queue and reopens on Done, exactly as the
empty path always did. A dead-worker fallback reclaims the tail buffer
from the unsent job (its bytes exist only there) and takes the classic
synchronous path. Epoll mode keeps the synchronous swap (its appends
write straight to the live fd and cannot be held through a worker-side
rename). Crash windows are unchanged: before the rename the live log
is intact; after it, the image is complete including the tail.

## Verification

Strings-only probe (4 × 20M-element giant collections preloaded,
restart for a fresh watermark, forced BGREWRITEAOF every 30 s, 4 min):

| | before | after |
|---|---:|---:|
| worst reactor tick | **120-188 ms** (n=5 runs, every cycle) | **50.5 ms** (= the no-rewrite control's noise floor) |
| completion arms ≥5 ms | `syncswap` every rewrite; one ~283 ms | **none** |
| verdict vs the 100 ms bar | FAIL | **PASS** (8 forced rewrites, zero over-bar ticks) |

With this, the closing-soak boundary statement shrinks to: nothing —
forced BGREWRITEAOF on multi-GB-per-shard datasets now finishes without
an over-bar reactor tick; the auto path was already clean.

## Gates

kevy-persist 69 + kevy-rt 30 + persistence e2e 18 green; locgate /
clippy -D warnings clean; crashgate + perfgate-median + branch CI
recorded below before merge.
