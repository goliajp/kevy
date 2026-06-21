# Axis G — collection ops

> **v1.25 outcome (the bench shape was the problem)**
>
> Phase A decomposition: `.claude/notes/v125-deco-axis-g-sadd-pilot.md`.
>
> **R3 ★ bench-shape finding**: `redis-benchmark -t sadd` defaults to
> `-r 0`, meaning every SADD inserts the literal string
> `"element:__rand_int__"` — the set holds 1 member forever. valkey
> runs in `OBJ_ENCODING_LISTPACK` 1-entry mode for the entire bench
> (1 cache line). kevy's `KevySet` 16-slot Swiss table **cannot
> structurally match** a 1-cell listpack. The pre-v1.25 "tied 99-103 %"
> result was specifically vs that 1-entry shape.
>
> The real attack lever was different: `kevy/src/cmd.rs::rest()` was
> cloning **every member's bytes into an owned `Vec<u8>` per multi-arg
> command** (SADD, SREM, HSET, HMGET, HDEL, LPUSH, RPUSH, ZADD, ZREM,
> DEL, EXISTS). valkey's `setTypeAdd(set, objectGetVal(c->argv[j]))`
> hands the parsed sds directly from argv — 0 copies, 0 allocs.
>
> **Shipped in v1.25**:
> - G4 (`4ec1278`) — `cmd::rest_borrowed` + `Store::*_borrowed`
>   variants for all 11 multi-arg cmds. Kills N+1 mallocs/command.
>   Measured ≈ +1 % at c=50 -P 1 (within variance band — the per-op
>   µs save is < 1 % of c=50 wire RTT; the structural correctness
>   matters more than the bench number).
>
> **Deferred to v1.26**:
> - **G-A2 `SmallSet<SmallBytes, N ≤ 8>` inline encoding** — mirror
>   valkey's intset/listpack for small sets. Caveat: `Value` enum
>   has a 32 B cap; inline N may only fit 2-3 cells.
> - Re-bench with `-r 100k` to force valkey into `OBJ_ENCODING_HT`
>   so the comparison is structurally fair.

---

# Historical body (pre-v1.25 framing — bench-shape unrecognised)

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
