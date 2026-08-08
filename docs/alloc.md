# `kevy-alloc` — the opt-in allocator: what it buys, what it costs

kevy ships with the system allocator (glibc malloc) by default. A pure-Rust
span allocator, `kevy-alloc`, can be compiled in as the process-wide
allocator:

```
cargo build --release -p kevy --features kevy-alloc
```

It is a build-time choice — the allocator sits under every path, so there is
no runtime toggle. The default build does not carry it.

## What it buys

Measured on the reference bench box (io_uring, 8-shard), workload details in
`bench/FINDING-2026-08-07-balance-round-ra-rc.md`:

- **Fragmentation / RSS**: long-running churn workloads hold ~2.16× the live
  data size in RSS vs glibc's 2.40× — roughly a 10 % smaller resident
  footprint at steady state, and the gap widens with allocation churn.
- **Capacity headroom**: the smaller, more predictable footprint is what the
  tiering capacity model budgets against; on capacity-bound deployments the
  allocator is the difference between fitting and paging.
- **Disk / persistence / stability**: zero measured cost — crash, replication
  and disk gates run identically on both builds.

## What it costs

On **saturated collection-write angles** (a shard's owner thread pegged by
pipelined sadd/zadd/hset traffic), the allocator's fast path costs ~1.7× per
call vs glibc, which shows up as:

- sadd ~−10~−16 %, zadd ~−13 % throughput on those saturated angles
  (hash writes are exempt — small hash values live inline in the store and
  pay −0.2 %).
- Angles that are not allocation-dense (GET/SET/INCR/LPUSH, cluster, RESP
  compat) price at −2~−6 %; unsaturated servers typically see no measurable
  difference at all — idle headroom absorbs the per-call cost.

Full decomposition: `bench/PERF-DECOMP-2026-08-08-zadd-sadd-alloc-tax-split.md`.

## When to enable it

Enable `kevy-alloc` when:

- Memory capacity is the binding constraint (cache boxes, tiered windows,
  many-tenant consolidation) and a ~10 % RSS reduction buys real headroom.
- The workload is read-heavy or mixed — the measured production shapes
  (R4a corpus) are dominated by reads and aggregations, where the allocator
  is free.
- You run long-lived processes where fragmentation growth, not peak
  throughput, is what pages you at 3 a.m.

Stay on the default when:

- Sustained, pipelined set/zset write throughput is the headline metric and
  the owner shards run saturated.
- You want the binary the perf baselines are recorded against.
