# Axis C — high key churn

> **v1.25 outcome (supersedes the historical body below)**
>
> Phase A decomposition: `.claude/notes/v125-deco-axis-c-churn.md`.
>
> **R3 ★ flipped finding**: the original "SmallBytes saves the
> malloc → kevy wins" story is half right. The malloc saving is
> real, but it's absorbed by `live_entry_mut`'s 2-probe shape under
> default `maxmemory=0`. The two structural advantages that actually
> hold kevy to parity (and would otherwise put it at -6 % vs valkey)
> were **not in the original hypothesis**:
> - kevy's inline `Entry::expire_at_ns: Option<NonZeroU64>` ≈ 18 ns
>   saved per SET (vs valkey's separate `db->expires` 2nd hashtable
>   that requires a probe per `removeExpire`).
> - kevy skips valkey's `t_string.c:262::tryObjectEncoding` path,
>   which allocates a `robj` via `createEmbeddedStringObject` on
>   every SET ≈ 30 ns saved.
>
> **Shipped in v1.25**:
> - G3 F3 (`9d2c03f`) — hoisted the maxmemory>0 gate out of
>   `precheck_for_write` / `try_evict_after_write` to dispatch (skips
>   two `#[inline]` calls per SET on default unlimited memory).
> - G3 F2' (`9d2c03f`) — first-byte guard in `parse_canonical_i64`
>   (rejects the redis-benchmark default `"xxx"` in ~1 ns instead of
>   a full UTF-8 → parse → itoa round-trip).
>
> **Deferred to v1.26**:
> - **D-A1 / F1 single-probe `live_entry`** — blocked on `kevy-map`
>   needing a raw-entry API; the borrow checker forbids the 1-probe
>   shape without it. Estimated -15-20 ns/SET (and /GET).

---

# Historical body (pre-v1.25 framing)

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
