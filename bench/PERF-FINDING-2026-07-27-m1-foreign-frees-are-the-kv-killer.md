# M1: same-shard KV is clean; cross-shard KV pays 18–39 % — the foreign path is the killer

**Status:** MEASURED, with a clean discriminator. perfgate's own A/B
(allocator-on candidate vs allocator-off reference, same commit,
interleaved, order flipped per instance, 3 instances) fails — but the
*shape* of the failure names the subsystem.

## The verdict

| angle | topology | Δ |
|---|---|---:|
| `pinned_cluster_get` | conn → owner shard's own port: **same-thread** | **+0.3 %** ✓ |
| `pinned_cluster_set` | same | **−0.9 %** ✓ |
| `pinned_compat_get` | one REUSEPORT port, key pinned elsewhere: **~7/8 cross-shard** | **−38.5 %** ✗ |
| `pinned_compat_set` | same | −39.2 % ✗ |
| `legacy_8sh_get/set/incr/sadd/hset/lpush/zadd` | one port, cross-shard dominant | −17.6 … −28.4 % ✗ |
| `zalg_zinterstore` | same | −24.6 % ✗ |

The discriminator is exact. In cluster mode each of the eight benchmark
processes connects to the shard that owns its hashtag, so every
allocation and free stays on one thread — and the allocator costs
**nothing** (±1 %, inside the noise band). In compat/legacy modes the
connection lands on an arbitrary shard while the key lives on its owner,
so values are allocated on one thread and dropped on another — and the
same allocator costs 18–39 %.

**The per-op fast path is vindicated and the foreign-free path is
convicted.** This also cleanly separates the two open regressions:
pub/sub runs `--threads 1` (no foreign frees at all), so its
small-payload gap is a different animal, as its finding already
suspected.

## Why the foreign path plausibly costs this much (candidates, not verdicts)

1. **A cross-core CAS per foreign free.** `push_foreign` CASes the
   owning segment's list head. Under compat load, up to seven shards
   hammer the same segment atomics — an RFO ping-pong on every free.
   At the compat_get baseline (~52 ns/op), one contended cross-core CAS
   is enough to explain the loss on its own.
2. **glibc never pays this.** Its tcache pushes a foreign chunk onto the
   *freeing thread's own* cache — zero cross-core traffic — and the
   freeing thread **reuses that memory locally** for its next
   allocation. Our design round-trips every foreign slot home before
   anyone can use it again. That is a structural difference, not a
   tuning one: home-routing was chosen to keep ownership exact, and its
   price on a cross-shard workload was never measured until now.
3. **`drain_foreign` walks every segment.** On each slow-path miss and
   each reclaim tick, O(all segments) — with a large keyspace, that is
   thousands of headers touched per sweep.

Which of the three dominates is measurable (a counter for CAS retries, a
build with drain throttled) — that is the decomposition round, and it is
now well-posed where the pub/sub one is not.

## Where the trade stands after all three measurements

| axis | allocator on vs off |
|---|---|
| resident memory (the arc's target) | **−17 %** (2.40× → 1.98×) |
| same-shard KV | **±1 %** — clean |
| cross-shard KV | **−18 … −39 %** — fails C4 outright |
| pub/sub, 64 B / 16 B / 4 KiB | −16 % / −8 % / ~0 % — mechanism unknown |

On these numbers the allocator cannot be default-ON: C4 (KV and pubsub
must not regress *with it enabled*) fails decisively on any workload
where clients do not follow cluster routing — which is exactly the
compat surface kevy promises. The memory win is real and the same-shard
path is proven clean; the foreign path needs a redesign round (local
reuse of foreign slots, tcache-style, with the ownership accounting
that entails) before the trade can be re-weighed.

perfgate also recorded 0.4–7.7 % box drift on the reference itself since
the baseline was taken — the interleaved design absorbed it, which is
why these deltas are trustworthy at all.
