# FINDING 2026-08-12 — S3: the epoll/kqueue AOF writer lane

Branch `feature/s3-epoll-writer-thread`. RFC:
`.claude/rfcs/2026-08-12-s3-epoll-writer-thread.md`. Upstream: S1
(ring offload) / S2 (always CQE-gating) closed the uring lane; the
poll-based reactors — the Linux old-kernel fallback AND the macOS
main path AND the entire PR test matrix (every integration test
forces KEVY_IO_URING=0) — still blocked the reactor on every write
and, under `always`, on every fsync.

## Mechanism (one paragraph)

Appends queue on kevy-persist exactly as under the ring (same
`queued_seq` watermark, same trap fixes); a per-shard thread drains
them with sequential `write_all`s on a `try_clone`d O_APPEND handle —
byte-identical to the synchronous path's file, because on an O_APPEND
fd the queue's offsets were only ever bookkeeping. One mpsc channel is
the whole ordering discipline: a fsync submitted after the queue
drained provably covers every earlier record. Under `always`, replies
hold in `conn.output` until the covering fsync completes (`flush_conn`
gate; parked conns drop write interest so a level-triggered poller
cannot spin; the lane wakes the shard via the poller waker and held
conns flush directly). The S5 off-thread rewrite swap unlocks for
epoll by construction — its gate reads `queued_mode()`, not the
reactor type. Dead-lane fallback requeues any unsent chunk at the
queue FRONT (order preserved, nothing lost or doubled — regression-
tested) and reverts to the synchronous path.

## Verification

- **crashgate server-always ×2** (the cell was parametrized first —
  gate before code): auto-reactor AND explicit `KEVY_IO_URING=0`
  cells, baseline green on the old synchronous path (branch CI run 1)
  before any engine change; green after on box (auto 37,896 / epoll
  37,813 replied writes survived kill -9) and macOS kqueue (423/409).
  Full crashgate 33/33 both hosts.
- **Box A/B (ext4, KEVY_IO_URING=0, appendfsync=always)**:

| cell | classic (offload=0) | lane (S3) | Δ |
|---|---:|---:|---:|
| SET -c 50 (median of 3) | 478 rps | **10,540 rps** | **+22×** |
| SET -c 1 | 391 rps | 384 rps | ≈parity |

  Same triangulation as S2: c1 stays fsync-bound (a leaky gate would
  show everysec-like throughput), so the gate is engaged; concurrency
  gets the group-commit multiplier. Sequential parity is even cleaner
  than the ring path's (no extra ring round trips — one waker wake).
- **kevy-persist 72/72** (incl. requeue-front order/offset regression
  and the handle-clone contract) + kevy-rt 50/50, macOS + Linux.
- **The whole PR test matrix now exercises the lane** (integration
  tests force KEVY_IO_URING=0 and the lane defaults on): replication /
  feed_cdc / persistence e2e green on both hosts.
- **perfgate-median 12/12** (uring face — S3 touches none of it; the
  gate is the proof): recorded below before merge.

## Boundaries (recorded, accepted)

- Held bytes count toward CLIENT_OUTPUT_HARD_LIMIT: a slow disk under
  always can trip the protective disconnect — no false ack, same as
  the ring path.
- The fsync-policy switch (CONFIG SET appendfsync) settles the lane
  before applying — the owner handle's honest flush must not
  interleave with the clone's in-flight writes. (The ring path's
  equivalent seam under live-config switching is pre-existing and out
  of scope here; noted in the RFC resolution.)
- embedded is untouched (no reactor); the ring path is untouched.
