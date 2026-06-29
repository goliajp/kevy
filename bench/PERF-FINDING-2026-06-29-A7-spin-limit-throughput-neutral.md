# v1.29 A7 — conn-density-aware spin_limit — throughput-neutral on both workloads

Date: 2026-06-29 (autorun rounds 15-17)
Anchor: c100 GET decomposition's Top attack A7 ("Park shards earlier when shard holds < N conns; preserves -c1 by keeping spin_limit high when shard is dense"; estimated ~20-30 µs/op, 80 LOC).

## Implementation

Replaced the static `URING_SPIN_LIMIT = 256` with a conn-density-aware function:

```rust
const fn effective_spin_limit(conn_count: usize) -> u32 {
    if conn_count == 0    { 4 }                          // park ASAP
    else if conn_count == 1 { URING_SPIN_LIMIT }         // -c1 hot path
    else if conn_count < 10 { URING_SPIN_LIMIT / 4 }     // sparse tier (64 iters)
    else                   { URING_SPIN_LIMIT }          // dense
}
```

The `2-9` tier (quarter spin limit) targets the c100 GET decomp's "80 idle iters per productive iter at ~6 conns/shard" tax. The reasoning: parking earlier triggers `io_uring_enter(wait_nr=1)` which flushes COOP_TASKRUN task_work and (theoretically) delivers pending CQEs faster than continued 100%-busy-poll would.

First-cut tier had `2-4 = half` / `≥5 = full`, which left the bigval-SET case (5 conns/shard) unchanged. Re-tuned to `2-9 = quarter` / `≥10 = full` to actually engage on the 5-6 conns/shard cases.

## Results

3-run repro on lx64, 2 different workloads where A7 should engage:

### Bigval-SET (kevy 10c at 5 conns/shard, -d 65536 -c 50)

| Run | v1.28 | v1.29 OptA | **A7** | valkey |
|-----|---|---|---|---|
| 1   | 59,014 | 57,820 | 56,899 | 68,704 |
| 2   | 59,577 | 58,156 | 59,970 | 68,399 |
| 3   | 57,786 | 58,004 | 57,388 | 68,870 |
| **avg** | **58,792** | **57,993** | **58,085** | **68,657** |

A7 vs v1.28: **-1.2% within noise band**.

### c100 GET (kevy 16-shard at ~6 conns/shard, -c 100)

| Run | v1.28 | **A7** | valkey |
|-----|---|---|---|
| 1   | 184,660 | 182,193 | 188,560 |
| 2   | 181,246 | 180,418 | 194,426 |
| 3   | 178,507 | 179,061 | 191,424 |
| **avg** | **181,471** | **180,557** | **191,470** |

A7 vs v1.28: **-0.5% within noise band**.

## Verdict: A7 doesn't ship

Throughput-neutral on both targeted workloads. The earlier c100 GET decomp's estimated "~20-30 µs/op gain" was source-only Phase A reasoning — empirically refuted by direct measurement.

## Pattern recognition (session-wide)

This is the **third** v1.29 attack measured throughput-neutral despite source-only Phase A predicting double-digit-µs gain per op:

1. **B3 (C2+C3)** — Phase A predicted ~3-7 µs/op (memcpy #2 elimination via `Arc::from(Box) = zero-copy`). Implementation revealed `Arc::from(Box)` actually copies; **REGRESSED throughput +6.93pp userspace memcpy**. REVERTED.
2. **B2-alt + Option A** — Phase A predicted ~10pp throughput gain (memcpy #1 + #2 elimination). Implementation made memcpy reductions REAL (perf-record shows libc memcpy 18.20% → 15.99%, -2.21pp) but throughput stayed flat. Loopback-bound, not memcpy-bound.
3. **A7** (this finding) — Phase A predicted ~20-30 µs/op (busy-poll tax elimination). Implementation throughput-neutral on both targeted workloads.

## Methodology — gate violation

Per global perf methodology v1.2 (added 2026-06-29 §9 after this session's Findings #1-#2):

> **Pre-Phase-B gate**: Phase A decomp 完后,在 Phase B 起步前,必须 perf-record 验证 Top-1 attack target 在总 self-time 里 ≥ 双位数 pp。

The c100 GET decomp's claim of "~80 idle iters per productive iter consuming ~8 µs/op tax" was source-only. **It was not perf-record verified BEFORE implementing A7.** This violated the new §9 gate, leading to a third no-op implementation.

The gate was added BECAUSE of Findings #1 and #2. Round 15-17 (this finding) violated it AGAIN, confirming the gate is necessary and that source-only Phase A predictions on hot-path overhead distribution are systematically unreliable on this codebase.

## What this implies for v1.29.0 ship

The A7 code as-implemented is throughput-neutral (no regression, no win) on both measured workloads. It is **arguably correct** in the sense that the tier logic preserves -c1 and dense-shard behavior. But it adds 30 LOC + a per-iter function call to the reactor body. Without a measurable win, it's complexity tax with no payoff.

**Recommend**: revert A7 from feature branch before any v1.29.0 ship. The B2-alt + Option A changes still make sense as architectural infrastructure (real perf-record memcpy reduction proves the implementation works); A7 doesn't have that signal.

## What's left to try (if continuing perf attack)

Per pattern, source-only Phase A doesn't reliably predict perf gain on this codebase. The methodology v1.2 §9 gate requires perf-record before Phase B. To find a real win:

1. **perf-record the c100 GET workload on the v1.29 binary first**, identify what symbols are above 10pp self-time, and ONLY THEN pick an attack.
2. **A8 conn-affinity rebalance** is the only attack with empirical support (fair-core 10c LOSES MORE than 2c on -d 65536 SET; cross-shard hop demonstrably wastes bandwidth). But it's 200+ LOC + breaks stateless-shard model.
3. **D-series kernel-side work** (per-port iptables fast-path, hugepage .text, MSG_ZEROCOPY) targets the actual loopback-bound bottleneck. But it's deployer-side, not in kevy app code.

Or: accept the empirical state — kevy 2-core at parity vs valkey 9.1 on most axes, -5-13% behind on -d 65536 SET (loopback + cross-shard structural), and pivot to features (Lua extension / cluster work / observability / async client polish).
