# PERF-FINDING 2026-07-04 — IDX.QUERY per-connection ~2ms tail mode

**Status**: CLOSED 2026-07-05 (v3.4 tails). Root cause confirmed =
accept placement (the SO_REUSEPORT-chosen owning shard conflicts
with its extension fan-out role); the v1.30 `--accept-shards`
config eliminates it entirely. See "Resolution" below.

## Symptom

At 1M indexed rows on lx64, `IDX.QUERY … LIMIT 100` (≈4 KB replies)
shows p99 ≈ 1.9–2.2 ms on SOME connections while others measure
0.35–0.9 ms — same server instance, same query mix.

Decisive experiment (same instance, 6 fresh conns × 300 queries):

    conn0: p50=0.31 p99=2.18
    conn1..5: p50≈0.31 p99=0.35–0.37

The tail rides the CONNECTION. Constant magnitude (~1.9 ms plateau,
p999≈p99), phase-uniform vs the 100 ms tick.

## Refuted along the way

- **Co-tenant valkey preemption** — SCHED_FIFO for the server left
  the tail unchanged (and FIFO actively HURTS: busy-poll at RT
  starves net softirq; do not use).
- **Client artifact** — PING through the same client/box: p99
  0.09 ms.
- **Reply size / range span alone** — full span×LIMIT matrix at 1M on
  a good placement: all clean (0.07–0.57 ms). 200k rows: clean.
- **Tick phase** — worst-10 timestamps uniform mod 100 ms.
- **Nagle / delayed ACK** — NODELAY set on both sides.

## Open hypothesis

Accept/RSS placement: the affected conn's softirq/processing CPU
coincides with a busy-poll shard core (or the conn's owning shard
occupies a pathological role in the extension fan-out), so ~1 % of
its replies wait a CFS timeslice (~2 ms) behind the pinned busy-poll
thread. Next probes: map bad-conn → owning shard identity (CLIENT
INFO / per-shard conn counters) across trials; check whether the bad
conn is always the accept-shard 0 conn; `/proc/softirqs` deltas per
core during a bad-conn run.

## Reproducer

`bench/idxgate.sh` internals; or boot 8-shard server, load 1M rows
`g:{i} ts=i`, build an i64 range index, then per fresh conn run 300×
`IDX.QUERY g_ts RANGE lo lo+20000 LIMIT 100` and compare per-conn
p99.

## Resolution (2026-07-05, v3.4)

Decisive experiment — same 1M-row workload, 8 fresh conns × 300
queries, three accept configurations:

    default        per-conn p99: 2.12 0.36 0.37 0.45 0.42 0.46 0.35 0.34
    --accept-shards 1              0.41 0.36 0.35 0.33 0.31 0.32 0.32 0.38
    --accept-shards 2              0.44 0.33 0.32 0.33 0.32 0.35 0.33 0.33

The tail exists ONLY under all-shard accept and vanishes completely
under a restricted accept set — confirming the placement hypothesis:
with every shard accepting, SO_REUSEPORT can land a connection on a
shard whose fan-out role (origin aggregation vs worker) or softirq
core placement makes ~1% of its replies wait a CFS timeslice.

**Operational answer**: serving-shape deployments should set
`--accept-shards` (the v1.30 recommendation `ceil(conns/25)..
ceil(conns/15)` already covers this). The gate keeps the
median-connection protocol; the finer kernel-level attribution
(softirq overlap vs origin dual-role) is not pursued further — the
config-level cure is total.

