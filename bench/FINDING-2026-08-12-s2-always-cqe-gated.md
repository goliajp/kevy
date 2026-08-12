# FINDING 2026-08-12 — S2: appendfsync=always, CQE-gated replies

Branch `feature/s2-crashgate-server-always`. RFC:
`.claude/rfcs/2026-08-12-s2-always-cqe-gated.md` (design + the
implementation Resolution). Upstream context: the v5 release plan
booked S2 as post-v5 ("always = synchronous is a semantic, not a
gap"); this arc removes the gap without touching the semantic.

## The invariant, restated as machinery

> No write's reply bytes leave the machine before its records are
> fsync-proven durable.

Before: the io_uring reactor BLOCKED on flush+sync_data once per recv
batch (aof_txn.rs end_group), so the invariant held by construction
and the reactor paid every fsync's latency on the hot path; AOF
offload refused `always` outright.

After: appends queue to the ring like everysec. Each dispatch that
appends records stamps the conn with the queued-record watermark; the
arm loop refuses to move `conn.output` into a write SQE until the
shard's fsync-proven durable watermark passes the stamp. The fsync is
submitted as soon as the queue and ring drain with any record
unproven, so concurrent conns' writes share one fsync round — group
commit falls out of the design rather than being bolted on. Cross-
shard ResponseBatches are held on the executing shard the same way.

## Verification

- **crashgate server-always cell** (new, the arc's first commit — the
  baseline ran green on the OLD synchronous path in CI before any
  engine change): real kevy binary, appendfsync=always, SIGKILL
  mid-write, every replied write must survive restart.
  - Baseline (sync path, CI Linux): green.
  - Gated path (box, io_uring): green — all 38,965 replied writes
    survived kill -9. (Instrument note: crashgate's WORK dir is /tmp
    = tmpfs on the box, so fsync is free there and the number is
    ring-round-trip speed, not disk speed. kill -9 preserves the page
    cache either way — the cell catches ordering/replay bugs, not
    fsync omission; the latency triangulation below is the fsync-half
    evidence.)
- **kevy-persist 70/70** (incl. the new queued+Always trap regression,
  red-green verified) + kevy-rt 50/50 on Linux.
- **A/B (box, redis-benchmark SET -d 64, appendfsync=always)**:
  classic = `KEVY_AOF_OFFLOAD=0` (synchronous fsync-before-reply),
  gated = default (S2). The c1 row doubles as gate-engagement
  evidence: a broken (leaky) gate would show c1 at everysec-like
  throughput; a working gate keeps c1 fsync-bound.

| cell | classic (offload=0) | gated (S2) | Δ |
|---|---:|---:|---:|
| SET -c 50 (median of 3) | 353 rps | **8,273 rps** | **+23×** |
| SET -c 1 (redis-benchmark) | 380 rps | 194 rps | −49% |
| SET seq (python, 3 runs) | ~200 ops/s | ~143 ops/s | −29% |

The three sequential datasets triangulate the gate as ENGAGED, not
leaky: on ext4 both paths are fsync-bound (5 ms/op classic, 7 ms/op
gated — the extra ~2 ms is the write-CQE → fsync-SQE → fsync-CQE →
next-arm round trips), while on tmpfs (the crashgate cell) the same
gated path runs at 51 µs/op because the fsync itself is free there. A
leaky gate would have shown ext4-gated at everysec-like throughput; it
shows fsync-bound instead. The single-sequential-writer latency tax is
the honest trade of S2 (per-op latency ≈ fsync + ≤1 tick, as the RFC
predicted); any concurrency at all flips it — at c50 the shared fsync
rounds give 23×, and the reactor never blocks, so mixed workloads keep
their read latency (the S4/S5 tail machinery now applies to always).

- **perfgate-median 12/12 PASS** (box, floor 0.92, n=3 medians; worst
  angles lpush −4.6% / zadd −4.4%, both inside the known floor+band —
  the everysec default path's regression gate for the restructured
  fsync scheduling).
- **Branch CI green.** Two unrelated flakes archived on the way
  (bench/.flake-archive/): availgate failover-convergence SECOND
  occurrence (--no-aof, unreachable; dual-primary transient named) and
  stream_groups XPENDING total≠per-consumer-sum FIRST occurrence
  (failed on a docs-only commit, passed on the engine commit; 40/40
  local loop green — carries an inconsistent-state signature worth a
  PEL-serialize look on recurrence).

## Boundaries (unchanged semantics, recorded)

- epoll keeps the synchronous fsync-before-reply path (S3's lane);
  embedded always fsyncs on the caller's thread as before.
- Pushed data (pub/sub, blocking-list serves) is not durability-gated
  — same as the synchronous path and valkey semantics: the gate
  covers write-command replies.
- On fsync failure the watermark stays put: held replies stay held
  (client may time out; no false ack) and the tick resubmits. This is
  stricter than the old sync path, which logged and replied anyway.
- run_swap now syncs the AOF directory after the rename (the data was
  always sync_all'd; the name linkage was the one undurable link in
  the swap-as-durability-proof chain). A dir-sync failure logs loudly
  but does not un-commit a landed rename.
