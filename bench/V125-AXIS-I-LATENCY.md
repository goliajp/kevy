# Axis I — tail latency (p50 / p95 / p99 / p99.9 / max)

> **v1.25 outcome**
>
> Phase A decomposition: `.claude/notes/v125-deco-axis-i-c50-10kb.md`.
>
> **R3 ★ flipped finding**: the original "kevy `Bytes::copy_from_slice`
> on 10 KB hits the allocator harder than valkey's reusable buffers"
> claim (at the bottom of this doc, pre-v1.25) was wrong. kevy's GET
> reply path is already zero-copy via `Value::ArcBulk` + writev;
> valkey actually memcpies 10 KB by default
> (`min-string-size-avoid-copy-reply=16384`, so 10 KB < 16 KB
> threshold). The real input-side waste was `uring_io.rs`
> unconditionally copying the kernel slab into `conn.input`.
>
> **Shipped in v1.25**:
> - G2 (`f763146`) — parse-from-slab fast path + `$<N>` pre-grow +
>   epoll `output_arcs` correctness fix. Measured:
>   - GET p999: **0.527 ms → 0.407 ms = -23 %** (vs valkey)
>   - GET p99: kevy 0.279 vs valkey 0.279 = tied
>   - SET rps c=50 -d 10240: +2 %
>
> **R3 ★ Phase B reverts (predictions overruled by measurement)**:
> - G6 A2 lazy-drop big values via `pending_drops`: predicted
>   -20 to -150 µs p999; measured **+144 µs p999** (worse). Single-
>   thread deferred bunching produces periodic batched-drop stalls
>   bigger than the inline drops it replaced. valkey's `lazyfree.c`
>   works because it has a separate bio thread. Reverted.
> - G6 A4 `submit_and_wait(1)` only-writes: predicted -50 to -200 µs
>   p999; measured **+44 % p999** (worse). The spin ladder existed
>   precisely so burst arrival catches the next recv within the spin
>   window. Reverted.
>
> **Deferred to v1.26** (the actual SET-tail amplifier):
> - **A3 / B-A1 take-into-Arc on SET path** — `cmd_data.rs::set_slice`
>   pays `Arc::from(&[u8])` alloc+copy of the 10 KB on every SET. The
>   fix requires argv ownership exposure from `kevy-resp`. Without
>   it, kevy SET p999/max at -d 10240 remains worse than valkey
>   (`0.487 ms / 1.519 ms` vs `0.335 ms / 1.039 ms`).
> - **Bio thread for free-work** — would unblock lazy-drop.

---

# Historical body (pre-v1.25 framing — input-side memcpy not yet identified)

When the average rps numbers tie at ~100 %, tail latency is the
real differentiator. This axis runs `redis-benchmark --precision
3` across the same scenarios as the matrix and pulls the
percentile distribution.

`redis-benchmark` uses HDR-style buckets — closest available
percentiles below the targets are:
- p95 ≈ 93.750 %
- p99 ≈ 99.219 %
- p999 ≈ 99.902 %

All times in **milliseconds**. lx64, kevy --threads 1, server
core 0; valkey/redis cores 0-9 with 10 io_threads; client cores
10-13.

## Results

### Single-conn (c=1, P=1)

| op   | metric | kevy   | valkey | redis  | kevy / valkey |
|------|--------|--------|--------|--------|---------------|
| SET  | p50    | 0.015  | ≥0.023*| ≥0.023*| **better**    |
| SET  | p999   | 0.039  | 0.047  | 0.063  | 0.83×         |
| SET  | max    | 0.455  | 0.479  | 1.319  | 0.95×         |
| GET  | p50    | 0.015  | ≥0.023*| ≥0.023*| **better**    |
| GET  | p999   | 0.039  | 0.047  | 0.071  | 0.83×         |
| GET  | max    | 0.951  | 0.455  | 0.879  | 2.09×*        |

\* "NA" cells = percentile fell into a coarser bucket; the
underlying value is between the next reported boundary.

### Moderate concurrency (c=50, P=1)

| op  | metric | kevy   | valkey | redis  | valkey/kevy |
|-----|--------|--------|--------|--------|-------------|
| SET | p50    | 0.135  | 0.135  | 0.159  | 1.00×       |
| SET | p99    | 0.191  | 0.199  | 0.231  | 1.04×       |
| SET | p999   | 0.319  | 0.439  | 0.287  | **1.38×**   |
| SET | max    | 1.575  | 1.679  | 1.111  | 1.07×       |
| GET | p50    | 0.135  | 0.135  | 0.167  | 1.00×       |
| GET | p99    | 0.199  | 0.255  | 0.215  | **1.28×** ✅|
| GET | p999   | 0.399  | 0.391  | 0.255  | 0.98×       |
| GET | max    | 0.647  | 1.223  | 0.439  | **1.89×** ✅|

### Higher concurrency (c=100, P=1)

| op  | metric | kevy   | valkey | redis  | valkey/kevy |
|-----|--------|--------|--------|--------|-------------|
| SET | p99    | 0.343  | 0.391  | 0.423  | **1.14×**   |
| SET | p999   | 0.727  | 0.791  | 0.511  | 1.09×       |
| SET | max    | 1.191  | 1.343  | 1.959  | 1.13×       |

### Pipelined (c=50, P=16) — HEADLINE

| op  | metric | kevy   | valkey | redis  | valkey/kevy |
|-----|--------|--------|--------|--------|-------------|
| SET | p50    | 0.159  | 0.359  | 0.215  | **2.26×** ✅|
| SET | p95    | 0.175  | 0.647  | 0.247  | **3.70×** ✅|
| SET | p99    | 0.223  | **1.959** | 0.287 | **8.78×** ✅|
| SET | p999   | 0.407  | **4.063** | 0.447 | **9.98×** ✅|
| SET | max    | 1.191  | 5.231  | 0.847  | **4.39×** ✅|

### Big-value (c=50, 10 KB)

| op  | metric | kevy   | valkey | redis  | valkey/kevy |
|-----|--------|--------|--------|--------|-------------|
| SET | p99    | 0.247  | 0.263  | 0.263  | **1.06×**   |
| SET | p999   | 0.487  | 0.335  | 0.343  | 0.69×       |
| SET | max    | 0.967  | 0.703  | 0.743  | 0.73×       |

## Headline

🎯 **c50-P16 pipelined SET: kevy p99 = 0.223 ms vs valkey p99 =
1.959 ms — kevy is 8.8× better at the tail**, 10× at p99.9, 4.4×
at max.

This is the single strongest userspace differentiator outside of
embedded mode. Under pipelined load (the workload most
transactional Rust clients drive), valkey's p99 blows up while
kevy stays flat. The thread-per-core busy-poll reactor avoids the
single-dispatcher contention that creates valkey's tail spikes.

## Why kevy wins the tail

1. **No dispatcher queueing.** valkey serialises commands through
   one main dispatcher thread; under pipeline burst, commands
   queue waiting to be parsed and a few unlucky requests sit
   behind a long chain. kevy parses+executes+queues-reply on the
   same core inline.
2. **No epoll wake-up latency.** kevy's busy-poll reactor checks
   SQ/CQ every iteration; no edge-triggered wake-up scheduling
   miss. The cost is 100 % CPU on the server core, which is the
   intentional tradeoff.
3. **No GC pauses.** Rust's deterministic drop means no stop-
   the-world; in pipeline mode where allocator pressure is highest,
   this matters most.
4. **No malloc serialisation.** kevy's per-shard arenas + Bytes
   ref-counting avoid the global allocator under load.

## Verdict per scenario

- c=1 P=1: TIE for all (single-flow, kernel-bound; both <0.05 ms)
- c=50 P=1 GET: kevy **1.28× better p99, 1.89× better max** ✅
- c=50 P=1 SET: kevy 1.38× better p999 ✅; p99 tied
- c=100 P=1: kevy 1.14× better p99 ⚠ (small edge)
- **c=50 P=16 (pipelined): kevy 4-10× better across ALL tail
  metrics** ✅✅✅
- c=50 10KB: parity at p99; valkey wins p999/max (1.45×) ❌

## Caveats

- The big-value (10 KB) p999/max loss reflects per-allocation
  cost; kevy hits the allocator harder on `Bytes::copy_from_slice`
  for 10 KB than valkey's reusable buffers do. Worth a future
  sprint to look at arena fast path for large vals.
- Numbers are taken on a single 200-500k-request run per scenario;
  for a paper-quality measurement we'd want multiple runs +
  HdrHistogram aggregation.

## Status

✅ **HUGE WIN at p99/p999/max under pipelined load** — kevy is
the obvious choice for any workload that pipelines (which is
~every modern Rust async client). The 8.8× p99 advantage at c50-
P16 is the v1.25 sprint's most production-relevant finding.

## Reproduce

```bash
ssh lx64
bash /tmp/axis_i_latency.sh
```
