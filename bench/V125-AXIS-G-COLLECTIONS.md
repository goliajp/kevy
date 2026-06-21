# Axis G — collection ops

**Hypothesis**: kevy's KevyMap (Swiss table) for hash/set/zset
backing vs valkey's listpack-or-dict hybrid might give differential
performance at certain collection sizes. Predict ≥120 % somewhere.

## Methodology

- `redis-benchmark -t {sadd,hset,zadd,lpush,rpush,lrange_100/300/600}`
  `-c 50 -P 1 -n 500 000`
- kevy `--threads 2`; valkey/redis `--io-threads 10`. Same lx64.
- 3 runs per cell, median.

## Result

| op          | kevy    | valkey  | redis   | **kevy / best** | verdict |
|-------------|---------|---------|---------|-----------------|---------|
| SADD        | 194 704 | 195 925 | 161 603 |  99 %           | tied    |
| HSET        | 195 312 | 190 259 | 166 889 | 103 %           | edge    |
| ZADD        | 195 312 | 191 424 | 163 239 | 102 %           | edge    |
| LPUSH       | 195 084 | 194 628 | 171 585 | 100 %           | tied    |
| RPUSH       | 197 550 | 192 901 | 165 892 | 102 %           | edge    |
| LRANGE_100  | 106 883 | 106 474 |  96 974 | 100 %           | tied    |
| LRANGE_300  |  43 014 |  42 764 |  39 939 | 101 %           | tied    |
| LRANGE_600  |  24 645 |  24 580 |  23 177 | 100 %           | tied    |

## Interpretation

**Hypothesis NOT confirmed.** kevy and valkey are within ±3 %
across every collection op tested. Both backends:
- store small collections inline (kevy: SmallBytes inline; valkey:
  listpack); the per-op cost difference is sub-noise
- transition to dict / KevyMap for larger collections; both
  implementations are Swiss-table-like O(1) inserts
- per-cmd RTT dominates at c=50-P1

LRANGE workloads cap at ~24-107k ops/s because each LRANGE reply
is a sized multi-bulk frame; kernel sendmsg per reply scales with
frame size, same for both servers.

## Honest verdict

**Tied.** Collection ops do not differentiate kevy vs valkey at
this bench shape — both have absorbed the relevant optimisations.
kevy wins clean over redis 8.8 (~12-20 % across the board) but
doesn't cross ≥120 % vs valkey.

## Reproduce

```bash
ssh lx64
bash /root/kevy/bench/axis_g_collections.sh
```

## Status

❌ **HYPOTHESIS NOT CONFIRMED.** Collection ops are at parity with
valkey across all eight tested operations.
