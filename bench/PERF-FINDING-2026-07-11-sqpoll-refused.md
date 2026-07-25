# SQPOLL on the shard reactor — REFUSED (measured verdict)

Date: 2026-07-11 · Box: lx64 (Debian 13, kernel 6.12.90, 16 cores) ·
Binary: feature/v4 working tree, `--threads 10` pinned cores 0-9,
client `redis-benchmark -P 16` pinned cores 10-15, `--no-aof`.
Judgment bar: keep the switch ON by default only on a ≥3% win.

## Setup

A/B via `KEVY_SQPOLL=1` (`IORING_SETUP_SQPOLL`, idle 1000 ms, no CPU
pin), added as a measurement-only env opt-in in
`kevy-rt/src/uring_setup.rs::build_uring`. Same pinning both sides.
SQPOLL engagement verified: 10 `iou-sqp` kernel threads present under
the flag, 0 without it. No foreign benchmark ran during either side
(preflight wait + post-run contamination check both clean).

## Numbers (median of 5, requests/sec)

| cell     |      base |  sqpoll | delta |
|----------|----------:|--------:|------:|
| GET c50  | 2,134,472 | 297,575 |  -86% |
| SET c50  | 2,098,636 | 361,991 |  -83% |
| GET c100 | 2,244,669 | 811,359 |  -64% |
| SET c100 | 2,212,390 | 798,722 |  -64% |

Raw runs in the session log; spread on the sqpoll side is wide
(c50 GET 247k-596k) — the kernel poll threads' scheduling makes the
loss itself noisy, but every single sqpoll run is far below every
single base run, so no variance argument survives.

## Why (mechanism, consistent with the prior attack-log entries)

One `iou-sqp` kernel poll thread spawns per shard ring; it inherits
the creating task's cpumask, so 10 poll threads spin on the same
cores 0-9 the 10 shard threads busy-poll on — effective CPU per shard
halves and the loss compounds with contention (worse at c50 than
c100 because the smaller batch size leaves less useful work per
context switch).

## Verdict

**REFUSED as a default** — loses 64-86% across every measured cell;
the bar was +3%. The `KEVY_SQPOLL=1` switch stays (default OFF) so
the A/B is one env var away whenever the layout assumptions change
(spare-core deployments, dedicated-poll-core pinning via the
`new_sqpoll` CPU parameter, future kernels).
