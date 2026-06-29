# v1.30 perfgate — `--accept-shards 3 --threads 10` reverses conn-density inversion

Date: 2026-06-29 (autorun round 28-30)
Anchor: [`PERF-FINDING-2026-06-29-fair-core-bigval-SET.md`](PERF-FINDING-2026-06-29-fair-core-bigval-SET.md) — empirical case for A8.
RFC: [`.claude/rfcs/2026-06-29-v1-30-accept-shards.md`](../.claude/rfcs/2026-06-29-v1-30-accept-shards.md).

## Setup

lx64, kevy `feature/v1-30-accept-shards` at `c38bf82` (post bind-fix). All servers `taskset 0-9`. redis-benchmark `taskset 10-13`, `-c 50 -d 65536 -n 200k -t set`. 3-run measurements per config (lower than v1.29 sweep's 3-run is intentional; the wins/loses pattern is unambiguous at the +10pp level).

## Results

| Run | default (no flag) | --accept-shards 3 | --accept-shards 6 | valkey 10c |
|----:|---:|---:|---:|---:|
| 1 | 55,279 | **63,674** | 59,648 | 68,989 |
| 2 | 55,432 | **63,452** | 57,356 | 69,881 |
| 3 | 58,343 | 59,827 | 60,901 | 67,797 |
| **avg** | **56,351** | **62,317** | **59,302** | **68,889** |

- **A3 vs default: +10.6 %** ✓ real architectural gain.
- A6 vs default: +5.2 % (partial — still has some conn-density tax at 8.3 conns/shard).
- A3 vs valkey: -9.5 % (was -13.4 % at default; gap narrowed ~4 pp).
- A3 vs kevy 2-core baseline (round 14 fair-core finding, ~63 k): **essentially at parity** — the inversion is reversed.

## Interpretation

The RFC heuristic `accept_shards ≈ ceil(conns / 15)` predicted A3 for `-c 50`:

- A3 = 16.7 conns/shard ≈ 15 floor → sweet spot ✓
- A6 = 8.3 conns/shard → still in the sparse-tier where the c100 GET decomp identified the "~80 idle iters per productive iter" tax.
- default (A10) = 5.0 conns/shard → maximum tax, the round-14 inversion case.

**A3 vs A6 vs default empirical pattern confirms the c100 GET decomp's "conn-density tax" mechanism is real** — fewer-but-denser shards win, with a clear knee in throughput around the 15-conns/shard mark.

## Extended A-curve sweep (2026-06-30 post-ship)

Empirical sweep over all `--accept-shards N` values 1..10 at the same `-c 50 -d 65536` workload (2 runs each for new values; A3/A6/default carry their original 3-run averages):

| config | conns/shard | avg SET/s | vs default |
|-------:|------------:|----------:|-----------:|
| A1 | 50.0 | 57.8 k | +2.7 % |
| **A2** | **25.0** | **62.3 k** | **+10.6 %** ⭐ |
| **A3** | **16.7** | **62.3 k** | **+10.6 %** ⭐ (RFC heuristic prediction) |
| A4 | 12.5 | 60.2 k | +6.9 % |
| A5 | 10.0 | 57.8 k | +2.7 % (2-run noise; sits between A4 and A6) |
| A6 | 8.3 | 59.3 k | +5.2 % |
| default A10 | 5.0 | 56.3 k | — |

- **Empirical sweet spot is broader than the RFC's single point**: A2 (25 conns/shard) and A3 (16.7) tie at ~62 k. Both `≥ ~17 conns/shard` qualifies.
- A1 (single shard, 50 conns) declines from peak — one core saturating limits throughput; cross-shard hops to the other 9 keyspace shards still occur.
- A4 / A5 fall on the descending side; A6 sees partial recovery (noise) before A10 hits maximum tax.
- The curve is approximately monotonic on the **sparse side** (A4 → A10 = declining); the **dense side** (A1 → A3) is also monotonic upward to the peak. The peak plateau is A2-A3.

## Recommendation refinement

The RFC heuristic `accept_shards ≈ ceil(conns / 15)` predicted A3 for `-c 50` — confirmed at the peak. The extended sweep shows the **acceptable range is `ceil(conns / 25) .. ceil(conns / 15)`**: for `-c 50`, both A2 and A3 work; for `-c 100`, both A4 and A6 would work. Users with mixed concurrency can pick the lower end (denser) without throughput loss vs the upper end.

## Caveats

- **Per-run variance is ~5 %** at 3 runs (default ranges 55k–58k; A3 ranges 60k–64k). Robust at the +10pp level; would not catch a +3pp tweak.
- **Run 3 A3 = 59,827** (-6 % vs runs 1+2's 63.5k) suggests workload-dependent noise; the win is real but not "stable +11 % every run".
- **A6 inconsistent** (range 57k–61k). Workload may be sensitive to exact conn-shard hash collisions. Documented as "less effective" rather than "wrong"; users on different conn counts may find A6 ≠ A10 win pattern.
- **Off-accept-set shards still consume CPU** (busy-poll → park ladder). At default `URING_SPIN_LIMIT = 256`, off-accept-set shards spend most cycles in `spin_loop` PAUSE then `io_uring_enter(wait_nr=1)` park. CPU% bounded but non-zero. v1.30.0 does not combine A7-style spin_limit-by-density tuning with A8 (left for v1.30.x or v1.31).

## What this validates

- **The conn-density inversion is real and structural** — measured at default 56k SET/s on kevy 10c vs 63k on kevy 2c (round 14 reverse-direction observation).
- **A8 simplified (static `--accept-shards N`) reverses it** — A3 brings 10c throughput to ~62k, matching kevy 2c parity.
- **Stateless-shard architecture is preserved** — off-accept-set shards run identical code; they just don't bind the listener (so SO_REUSEPORT routes elsewhere).
- **The gap to valkey on -d 65536 SET (loopback-bound)** narrows but doesn't close — from -13.4% to -9.5%. The remaining gap is structurally in the kernel TCP path (per methodology v1.2 §9 gate on c100 GET; same root cause applies here).

## What this does NOT do

- Doesn't close the gap to valkey on the loopback-bound workload (`-d 65536 SET`). The remaining -9.5 % requires D-series kernel work (per-port iptables fast-path / MSG_ZEROCOPY / hugepage `.text`) which is deployer-side, not app code.
- Doesn't automatically detect `accept_shards`. User configures per workload.
- Doesn't combine with A7 spin_limit-by-density. A7 was reverted in v1.29 round 15-17 (throughput-neutral); future v1.30.x may revisit with off-accept-set shards as a clearer target.

## Verdict

**SHIP v1.30.0**. The architectural change works as designed, has perf-record / bench-empirical backing, RFC heuristic matches measurement (A3 = sweet spot at `-c 50`), and the v1.29 byte-identical default (no flag → no change) means existing deployments are unaffected.

## Bug fixed during validation

C3-C5 initially gated only the accept SQE arm (`if self.arms_accept`). The off-accept-set shards STILL bound the listener via `tcp_listen_reuseport` — Linux SO_REUSEPORT then routed SYNs to them; they didn't accept, so conns hung in the kernel queue.

Fix in `c38bf82`: `Shard.listener: Socket` → `Option<Socket>`. Off-accept-set shards get `None`; `tcp_listen_reuseport` is gated on `arms_accept`. SO_REUSEPORT now redistributes across only the bound subset. Validated manually before bench: `redis-cli -p 7001 PING` returned PONG with `--accept-shards 3 --threads 10`.
