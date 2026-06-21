# Axis H — pub/sub fan-out throughput

> **v1.25 outcome — biggest win of the sprint**
>
> Phase A decomposition: `.claude/notes/v125-deco-axis-h-pubsub-edges.md`.
>
> **R3 ★ two flipped predictions (both originally in this doc)**:
> - "subs=10 vs redis -16 % is SPSC batch amortisation below N=20"
>   → wrong. Under `--threads 1`, `nshards=1`, the SPSC fan-out
>   path is dead code. Real causes: O(N_conns) `conns.iter().filter()`
>   in `deliver_publish`, no dedup on `dirty.extend_from_slice(&ids)`
>   (10 K entries/burst), and a wasted Arc + 2 to_vec when nshards==1.
> - "size=4 KB -7 % vs valkey is 10-io-thread parallelism" → wrong.
>   Real lever is valkey's `bulkStrRef` (`networking.c::addReplyBulk
>   WithFlag(avoid_copy=1)`): 16 B handle per client + writev gather,
>   not memcpying the 4 KB. IO threads only ~5 µs of the 25 µs gap.
>
> **Shipped in v1.25** (G5 chain):
> - `4b72ec0` H1.A — nshards==1 fast path (skip Arc + 2 to_vec).
> - `6587032` H1.B + H1.C + H2.A — per-channel `subs_by_channel`
>   index, `pending_write` flag dedup, Arc-shared message body +
>   writev gather (kevy's `bulkStrRef` equivalent).
>
> **Bench (lx64, --threads 1, median of 3)** — same numbers in the
> master:
>
> | subs / size       | kevy / valkey     | kevy / redis |
> |-------------------|------------------:|-------------:|
> | subs=50  16 B     | 23.10 M / 5.11 M = **452 %** | 2.01× |
> | subs=100 16 B     | 28.38 M / 5.67 M = **500 %** | 2.41× |
> | subs=200 16 B     | 31.25 M / 6.27 M = **498 %** | 2.70× |
> | subs=500 16 B     | 31.68 M / 6.13 M = **517 %** | 3.02× |
> | subs=10  16 B     | 6.38 M  / 4.01 M = 159 %; vs redis 6.09 M = **105 %** (was 0.84×) |
> | subs=50  256 B    | 7.62 M  / 5.53 M = 138 % | 1.28× |
> | subs=50  4 KB     | 1.11 M / 2.26 M = **49 %** (LOSS — deferred) | 1.47× |
>
> **R3 ★ implementation-time finding (not in Phase A)**: Linux
> `IOV_MAX=1024` cap surfaced during H2.A bench — uncapped Arc
> accumulator hung the bench at subs ≥ 50 / size ≥ 256 because
> writev returned `-EINVAL` for ~3000 iovecs. Correctness fix:
> `PUBSUB_ARC_FLUSH_AT=256`. This means at subs=50 / 4 KB we still
> hit the cap and can only zero-copy 256 of every 1024 pipelined
> publishes per conn.
>
> **Deferred to v1.26**:
> - **H 4 KB writev-chunking** — split the iovec list across multiple
>   writev syscalls per drain when `IOV_MAX=1024` is the bottleneck.
>   Target: pub/sub size=4 KB rises from 49 % → ≥ 120 % vs valkey.

---

# Historical body (pre-v1.25 framing — both edge-case stories refuted)

Refresh of the v1.18-era 2.3× pub/sub finding against the v1.25
codebase, plus a fan-out sweep across subscriber counts and
payload sizes. **`kevy --threads 1`** vs **valkey 9.1.0** vs
**redis 8.8.0** on lx64 (i7-10700K, mitigations=off, server
core 0; client cores 10-13).

Bench harness: `crates/kevy-pubsub-bench` — pure-Rust subscriber
pool + pipelined publisher; metric = `delivered = subs × msgs /
elapsed` (the fan-out actually performed). Median of 3 runs.

## Results (delivered msg/s)

| scenario              | kevy        | valkey      | redis       | kevy / best | verdict |
|-----------------------|-------------|-------------|-------------|-------------|---------|
| subs=10  msgs=100k 16B|   5 171 007 |   4 047 202 |   6 171 627 | 84 % (redis)| ❌ LOSS |
| subs=50  msgs=100k 16B|  14 148 533 |   5 816 667 |  11 465 028 | 123 %       | ✅ ≥120%|
| subs=100 msgs=50k  16B|  15 953 966 |   6 116 122 |  11 969 867 | 133 %       | ✅ ≥120%|
| subs=200 msgs=20k  16B|  15 923 072 |   6 265 358 |  11 644 140 | 137 %       | ✅ ≥120%|
| subs=500 msgs=10k  16B|  13 503 117 |   6 139 064 |  10 570 591 | 128 %       | ✅ ≥120%|
| subs=50  msgs=50k  256B|  7 715 562 |   5 626 593 |   6 049 090 | 128 %       | ✅ ≥120%|
| subs=50  msgs=20k  4KB|     969 091 |   1 037 673 |     751 189 | 93 %        | ❌ LOSS |

## kevy / valkey only (the main competitor)

| scenario       | kevy / valkey |
|----------------|---------------|
| subs=10  16B   | 128 %         |
| subs=50  16B   | **243 %**     |
| subs=100 16B   | **261 %**     |
| subs=200 16B   | **254 %**     |
| subs=500 16B   | **220 %**     |
| size=256       | 137 %         |
| size=4096      | 93 % ❌       |

vs valkey alone: kevy is the obvious choice at subs ≥ 50 with
1.3×-2.6× advantage. Only the 4 KB payload fan-out edges valkey
(7 % gap, ~noise band but consistent across runs).

## What's special about subs=10 / size=4096

**subs=10 (LOSS to redis)**: at only 10 subscribers the per-flow
fan-out cost dominates, and the kevy `Publish` path (per-shard
broadcast → SPSC to peer shards → per-conn pump) adds latency
that redis's straight-loop fan-out doesn't pay. The advantage
flips once N is high enough that the SPSC batch amortises (≥ 50).

**size=4096 (LOSS to valkey)**: at 4 KB per message, the bench is
writev-bound rather than fan-out-bound. valkey's edge here is
likely better socket-buffer sizing or write batching across
subscribers. kevy's per-conn writev path doesn't batch across
subscribers — every subscriber's outbound has its own writev SQE.

## Comparison to history

The v1.18-era memory recorded "pub/sub 2.3× over valkey" — this
matches the **subs=50** number above (2.43×). The v1.24/v1.25 hot
GET/SET work did **not** touch the pub/sub path, so we neither
regressed nor gained. The fan-out range we hadn't measured
before (subs=200 reaches **2.54×**) and the medium-payload
(size=256, 1.37×) win are new data points.

## Headline

- ✅ **5 of 7 scenarios ≥ 120 %** vs best-competitor
- ✅ **subs=100 hits 261 % vs valkey** (15.95M vs 6.12M msg/s)
- ❌ subs=10 vs redis loses (-16 %); size=4096 vs valkey loses (-7 %)

The wins are concentrated where kevy was designed to win: many
subscribers needing batched fan-out. The losses are at extremes
(very low subs, very large payloads) where different code paths
dominate.

## Status

**No regression vs v1.18 baseline.** Pub/sub remains kevy's
strongest userspace differentiator (≥ 2× vs valkey across all
non-extreme scenarios). Two follow-ups for a future sprint:
- subs=10 latency-floor fix (SPSC batch amortisation threshold)
- size=4096 writev batching across subscribers

Neither blocks v1.25 since both are extremes outside the typical
pub/sub workload.

## Reproduce

```bash
ssh lx64
bash /tmp/axis_h_pubsub.sh
```

Or via the in-repo bench crate against any RESP server:

```bash
kevy-pubsub-bench --host 127.0.0.1 --port 7001 --subs 100 \
                  --msgs 50000 --size 16
```
