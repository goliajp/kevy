# Axis E — deep concurrency sweep

**Hypothesis**: shared-nothing thread-per-core + kevy-ring SPSC
scales linearly with conn count; valkey's single-dispatcher
saturates beyond ~1 k conns. Predict kevy ≥120 % at -c 500+.

## Methodology

- `redis-benchmark -c {50,200,500,1000,2000} -P 1 -t set,get -n 1M`
- kevy: `--threads 2` for c ≤ 100, `--threads 10` for c ≥ 500
  (matched to expected conn-per-shard ratio)
- valkey/redis: `--io-threads 10 --maxclients 65535`
- `ulimit -n 65536` on client side.
- 3 runs per cell, median.

## Result

| -c   | op  | kevy    | valkey  | redis   | **kevy / best** | verdict      |
|------|-----|---------|---------|---------|-----------------|--------------|
|   50 | SET | 193 648 | 193 611 | 165 948 | 100 %           | tied         |
|   50 | GET | 195 963 | 194 818 | 164 393 | 101 %           | ⚠ win<120 %  |
|  200 | SET | 189 609 | 187 723 | 173 010 | 101 %           | ⚠ win<120 %  |
|  200 | GET | 186 012 | 187 899 | 165 098 |  99 %           | tied         |
|  500 | SET | 175 963 | 182 083 | 155 715 |  97 %           | **❌ LOSS**   |
|  500 | GET | 177 557 | 187 266 | 156 961 |  95 %           | **❌ LOSS**   |
| 1000 | SET | 156 421 | 171 674 | 146 499 |  91 %           | **❌ LOSS**   |
| 1000 | GET | 151 584 | 176 523 | 147 189 |  86 %           | **❌ LOSS**   |
| 2000 | SET | 113 611 | 154 727 | 126 550 |  73 %           | **❌ -27 %**  |
| 2000 | GET | 111 508 | 153 917 | 128 667 |  72 %           | **❌ -28 %**  |

## Interpretation

**Hypothesis BUSTED.** The exact opposite of the prediction held —
kevy does NOT scale better than valkey at high conn count; instead
it **degrades faster**:
- c≤200: roughly tied (within ±2 %)
- c=500: kevy -3 to -5 %
- c=1000: kevy -9 to -14 %
- **c=2000: kevy -27 %**

### Root cause: `uring_arm_conns` is iterate-all

Every reactor iter, kevy walks every conn assigned to this shard
to (a) prep a write SQE if output is pending, (b) re-arm the
multishot recv if it terminated, (c) start a write swap if
needed. The work per conn is small (~50 ns) but it scales O(N).

At c=2000 with 10 shards: 200 conns/shard. arm_conns = 200 × 50 ns
= 10 µs per iter. At 110 k cmds/s ÷ 10 shards = 11 k iters/s per
shard, that's 110 ms/s of CPU JUST in arm_conns — **11 % of one
core, per shard**, just iterating idle conns.

valkey uses epoll which is **event-driven**: only conns with a
ready fd are processed. Idle conns cost zero per-iter.

This is the **fundamental scalability cliff**: kevy's busy-poll +
iterate-all model wins at low conn count (the per-iter admin is
amortised over enough work per conn) but loses at high conn count
(the admin cost becomes dominant).

## Honest verdict

**kevy LOSES Axis E by a wide margin at c ≥ 500.** This is a
structural disadvantage of the iterate-all busy-poll model.

The fix would be to maintain a **ready-set bitmap**:
- arm_conns only iterates conns with `output.is_empty() == false`
  OR recv-arm-needed
- A SET / GET reply on a conn adds it to the ready-set; arm_conns
  clears the bit once the write SQE is submitted
- recv-arm-needed is a one-time event (multishot recv terminates
  rarely)

This is medium-effort (several hours of careful concurrency work)
but would close the c=500-2000 gap. **Not in this v1.25 sprint.**

## What this means for the ≥120 % goal

**Axis E will NOT contribute to ≥120 % on high-conn workloads
without the ready-set refactor.** It's a known structural
limitation of the current reactor.

## Reproduce

```bash
ssh lx64
bash /root/kevy/bench/axis_e_concurrency.sh
```

## Status

❌ **HYPOTHESIS BUSTED — kevy LOSES at c ≥ 500.** Root cause is
the iterate-all `uring_arm_conns` loop; the fix is a ready-set
bitmap, deferred to a follow-up sprint.
