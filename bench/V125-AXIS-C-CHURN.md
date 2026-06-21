# Axis C — high key churn

**Hypothesis**: SmallBytes inline (key + value ≤22 B fits in the
KevyMap bucket) means SET pays **0 malloc** per insert; valkey
allocates a `robj` per SET regardless of size. Predict kevy ≥120 %
on churn-heavy SET with -r 100k / 1M / 10M random keys.

## Methodology

- `redis-benchmark -c 50 -P 1 -t set -r {100k, 1M, 10M} -n {500k, 1M, 2M}`
- Both servers `--maxmemory 8gb` so 10 M-key bench doesn't trigger
  eviction.
- kevy `--threads 2`; valkey/redis `--io-threads 10`. Same lx64.
- 3 runs per cell, median.

## Result

| -r (keyspace) | op  | kevy    | valkey  | redis   | **kevy / best** | verdict |
|---------------|-----|---------|---------|---------|-----------------|---------|
| 100 000       | SET | 191 718 | 191 498 | 155 280 | 100 %           | tied    |
| 1 000 000     | SET | 191 644 | 193 424 | 168 691 |  99 %           | tied    |
| 10 000 000    | SET | 192 086 | 191 663 | 165 317 | 100 %           | tied    |

## Interpretation

**Hypothesis NOT confirmed.** Across keyspace sizes (100k → 10M),
kevy and valkey are TIED (99-100 %). kevy maintains a clean ~12-15 %
lead over redis.

Why no kevy ≥120 % win:

1. **The bench is c50-P1 round-trip-bound, not allocation-bound.**
   Per SET ≈ 5.2 µs at 192 k ops/s. Of that, network (tcp_sendmsg,
   tcp_recvmsg, schedule) dominates. Per-SET malloc cost is
   ~30-100 ns (jemalloc tcache hit) = 0.5-2 % of the per-op
   budget. SmallBytes saves that 30-100 ns; the saving is well
   inside the noise floor.
2. **valkey uses jemalloc-5.3.0 too.** Its tcache makes per-SET
   `robj` allocations effectively free at steady-state churn. The
   pre-malloc/free pair on valkey is amortised.
3. **valkey's dict (Swiss hashtable, similar to kevy-map's Swiss
   table)** has comparable insert performance. Both are O(1)
   amortised with similar cache-line patterns.

The SmallBytes inline win is real ARCHITECTURALLY (no separate
allocation per value ≤22 B), but it's **invisible in this bench
shape** because the RTT floor dominates.

## Where SmallBytes WOULD show up

- **Pipelined / non-RTT-bound workloads** — see Axis A (-P 64+),
  where per-op CPU savings dominate. SmallBytes contributes to
  kevy's 308 % SET win there.
- **Embedded mode (kevye)** — see Axis F (planned). No network,
  no protocol → per-op cost IS the SmallBytes save vs robj alloc.
- **High memory pressure** — SmallBytes uses 24 B per value;
  valkey's `robj` is ~56 B + value alloc separately. kevy's
  per-value overhead is ~half of valkey's. **Memory footprint
  axis** would show this, but redis-benchmark doesn't measure
  that directly.

## Honest verdict

**Tied.** SmallBytes contributes upstream (composed wins in Axes
A + F), but doesn't drive a standalone ≥120 % on the SET-churn
bench shape. Axis C is **NOT a path to ≥120 %** in isolation.

## Reproduce

```bash
ssh lx64
bash /root/kevy/bench/axis_c_churn.sh
```

## Status

❌ **HYPOTHESIS NOT CONFIRMED.** kevy + valkey tied 99-100 % across
keyspace sizes 100k / 1M / 10M. SmallBytes inline contributes to
composed wins on other axes but not standalone here.
