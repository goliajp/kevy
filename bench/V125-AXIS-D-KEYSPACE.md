# Axis D — large keyspace TLB pressure

**Hypothesis**: E13 2 MiB-aligned mmap THP for kevy-map (verified
working — `AnonHugePages` in `/proc/PID/smaps` is populated for the
key bucket array) means GETs on a 10 M-key keyspace hit fewer TLB
misses than valkey's default-4K-page dict. Predict kevy ≥120 % on
random GET over 10 M warmed keys.

## Methodology

- Warm 10 M random keys with `redis-benchmark -t set -n 10000000
  -r 10000000 -c 50 -P 32`
- Both servers `--maxmemory 8gb`. kevy `--threads 2`,
  valkey/redis `--io-threads 10`.
- Bench: `redis-benchmark -t get -n 2 000 000 -c 50 -P 1 -r 10000000 -q`
- THP confirmation: read `/proc/PID/smaps` after warm, sum
  `AnonHugePages` (only meaningful for the kevy process).

## Result

| -r (warm keys) | op  | kevy    | valkey  | redis   | **kevy / best** | verdict |
|----------------|-----|---------|---------|---------|-----------------|---------|
| 10 000 000     | GET | 190 223 | 192 252 | 164 217 | 99 %            | ❌ -1 % |

**`kevys AnonHugePages_sum = 602 112 KB`** (≈ 588 MiB) — that's
294 × 2 MiB pages successfully promoted to THP. **E13's design
works as advertised — the keyspace map IS hugepage-backed.**

## Interpretation

**Hypothesis NOT confirmed at this scale.** Despite the THP
machinery doing exactly what it was designed to, the bench shows
kevy + valkey TIED (99 %).

Cost decomposition (back-of-envelope):
- Per-GET total: 1/192 000 ≈ 5.2 µs
- Of that, **network round-trip dominates** (~3 µs in kernel:
  tcp_sendmsg + recvmsg + soft-irq + schedule)
- **Keyspace lookup ≈ 100-200 ns** (bucket-array access at 10M
  keys is L3-miss → DRAM ≈ 100 ns)
- **TLB miss saved by THP ≈ 5-20 ns per lookup** = 5-20 % of the
  lookup itself, but only 0.1-0.4 % of the whole per-cmd budget.

THP wins where the keyspace lookup is the dominant cost, NOT
where network round-trip is. That means:
- **Single-process / in-memory workloads (Axis F kevye)** — should
  show the THP win cleanly.
- **Big-batch pipelined (Axis A -P 256)** — per-cmd network cost
  amortised, lookup cost surfaces.

For the c50-P1 RTT-bound bench, THP is verified working but
**sub-noise** in the bench output. The architectural lever is
correct; the bench shape doesn't expose it.

## What WOULD make THP show up in bench

- **Reduce network overhead**: pipelining (Axis A) or embedded
  (Axis F).
- **Increase lookup intensity**: SCAN over the full keyspace,
  multi-key MGET with random keys covering > 100 M cells, etc.
- **Cold-start GET (no warmup)**: forces every lookup to miss
  cache. But not a steady-state metric.

## Honest verdict

**E13 mechanism: working perfectly (588 MiB hugepage-backed).
Bench impact at -c 50 -P 1: invisible (RTT-bound).** Axis D
contributes to memory efficiency + composed wins on Axes A / F
but does NOT drive a ≥120 % standalone result here.

## Reproduce

```bash
ssh lx64
bash /root/kevy/bench/axis_d_keyspace.sh
```

## Status

⚠️ **HYPOTHESIS NOT CONFIRMED on this bench shape.** THP machinery
verified-working; bench shows TIED (99 %). The lever exists but
needs a different workload to surface.
