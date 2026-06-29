# Fair-core comparison: kevy 10c vs valkey 10c at -d 65536 SET

Date: 2026-06-29 (autorun round 14)
Anchor question (raised round 13): is the -d 65536 SET "-6% gap vs valkey" purely a kevy 2-core handicap, or is it structural?

## Setup

- kevy `--threads 10` (10 shards), `taskset -c 0-9`
- valkey 9.1 `--io-threads 10`, `taskset -c 0-9`
- Client: `redis-benchmark -c 50 -P 1 -n 200k -t set -d 65536`, `taskset -c 10-13`
- Each kevy invocation gets a fresh `--dir /tmp/kevy-bench-*` to avoid prior on-disk shard layout influencing startup

## Result (3 runs, median of 3 per run)

| Run | kevy v1.28 baseline (10c) | kevy v1.29 B2-alt+OptA (10c) | valkey 9.1 (10c) |
|-----|---------------------------|------------------------------|------------------|
| 1   | 59,506                    | 59,400                       | 68,422           |
| 2   | 60,042                    | 58,309                       | 67,842           |
| 3   | 58,445                    | 60,277                       | 69,348           |
| **avg** | **59,331**            | **59,329**                   | **68,538**       |
| sample stdev | ~660 (1.1%)         | ~825 (1.4%)                  | ~620 (0.9%)      |

**kevy 10-core average vs valkey 10-core average: 59,329 / 68,538 = 86.6% → kevy 13.4% BEHIND at fair-core.**

## Surprising — kevy's perf at -d 65536 SET is INVERSE in shard count

Cross-reference with prior 2-core kevy measurement on the same workload:

| Config | kevy avg | valkey avg | kevy / valkey |
|--------|----------|------------|---------------|
| kevy 2-core | 65,217 | 69,046 | 94.6% (-5.4%) |
| **kevy 10-core** | **59,329** | **68,538** | **86.6% (-13.4%)** |

**kevy at 10 shards is SLOWER than kevy at 2 shards** on this workload (-9% relative; -5,888 SET/s absolute). valkey at 10 io-threads is roughly unchanged. **More kevy parallelism HURT throughput at -d 65536 SET.**

This refutes the round 13 "kevy 2-core handicap" hypothesis. The gap isn't core count; it's structural to kevy's many-shard architecture on a key-uniformly-distributed write workload.

## Root cause hypothesis (consistent with c100 GET decomposition finding A8)

At `-c 50 / 10 shards`, each shard sees ~5 connections on average. The c100 GET decomposition documented at [`PERF-DECOMP-2026-06-28-c100-GET-vs-valkey-9.1.md`](PERF-DECOMP-2026-06-28-c100-GET-vs-valkey-9.1.md) §"What this means" identified the **conn-density tax**: at low conns/shard, the busy-poll body runs ~80 idle iterations per productive iteration, and the per-iter overhead dominates.

For SET specifically, this compounds with **cross-shard hop**: keys hash uniformly across shards, so on a 10-shard server, 90% of SETs land on a non-owning shard. The connection-owning shard packages the request into `Inbound::RequestBatch` and sends to the key-owning shard via the cross-shard channel; the owner runs the actual write and sends the reply back.

Net per SET at 10-shard:
- 90% probability of cross-shard hop (each adds channel send + receive + serialize argv)
- Each shard's busy-poll has only ~5 conns to amortize the per-iter overhead
- 10 shards multiply the per-iter overhead 10×

vs 2-shard:
- 50% cross-shard hop probability
- Each shard has ~25 conns (better amortization)
- Only 2× per-iter overhead

The result: 2-shard kevy is faster than 10-shard kevy on this workload. valkey doesn't have this inverted relationship because its io-threads model centralizes the keyspace and just farms out the kernel-side TCP work; all 10 io-threads share access to the keyspace via locks.

## What this validates from earlier work

The c100 GET decomposition's Top attack **A8 — conn-affinity rebalance** (estimated ~40-60 µs/op gain, "the structurally correct fix") is empirically supported by this fair-core data. The mechanism is:
- At `conns < shards`, fold conns onto fewer shards via SO_REUSEPORT BPF program or per-shard accept gating
- Preserves per-conn busy-poll efficiency
- Eliminates cross-shard hop overhead on hot-loaded shards

A8 was deprioritized in the c100 decomp as "200+ LOC, breaks stateless-shard model" but its expected gain (~40-60 µs/op) is well above the 13% gap measured here.

## Updated project standing perf claim

The 2026-06-29 round 13 claim ("kevy parity-or-ahead at every axis except `-d 65536 SET` which is loopback-bound") needs an honest qualifier:

- At kevy 2-core: kevy is 5.4% behind valkey 10-core. Most of the gap is loopback-bound + thread-count handicap.
- At kevy 10-core (fair-core): kevy is **13.4% behind** valkey 10-core. Loopback bandwidth saturates similarly, but kevy's cross-shard routing + low conn-density per shard adds an extra 8 pp of overhead.

The "fair" comparison reveals kevy's many-shard architecture has a **conn-density inversion** at this workload that the 2-core configuration mostly hides. v1.29 B2-alt+OptA is throughput-neutral at both configs — the bareset path memcpy reduction is real but dwarfed by the structural overheads.

## What v1.29.0 SHOULD claim if shipped

- Architectural prep landed (B2-alt prep_cancel infrastructure + Option A `Arc<Box<[u8]>>` value type)
- Userspace memcpy reduction verified via perf record (-2.21 pp at -d 65536 SET, perf-record only)
- No throughput regression on any measured axis (collections / pipelining / tail latency / GET / pubsub / small-payload SET)
- The -d 65536 SET specifically: throughput-neutral vs v1.28 at 2-shard config (-0.5%); throughput-neutral at 10-shard config (-0.003%, basically identical)
- **Does NOT close the kevy-vs-valkey gap on -d 65536 SET** — that gap is structural (conn-density + cross-shard hop at 10-shard; loopback + per-thread share at 2-shard) and requires the A8 architectural attack to close

This is honest. Not a perf headline release, but a structurally clean prep + Phase A re-decomposition findings recorded for the next sprint.

## Phase B next-target options (updated round 14)

1. **A8 — conn-affinity rebalance** (200+ LOC, breaks stateless-shard model): the empirically supported attack. Folds conns onto fewer shards when `conns < shards`. Expected gain ~40-60 µs/op → could close the 13% fair-core gap and put kevy ahead of valkey at -d 65536 SET.

2. **A7 — conn-density-aware spin_limit** (80 LOC, lower-risk): park shards earlier when they hold < N conns; preserves -c1 by keeping spin_limit high when shard is dense. Partial closure (estimated ~20-30 µs/op).

3. **Smaller-shard-count default for large-payload workloads**: detect workload shape at runtime and dynamically shrink shard count? Probably not — kevy's thread-per-core is a foundational design constraint.

4. **Accept the structural -13% at fair-core, ship v1.29.0 with the honest qualifier** — and pivot to features (Lua / cluster / observability) next sprint.
