# kevy-map performance budgets

Per-op ns numbers + reproducer commands. **Ratios are the signal**; absolutes
drift with host load and arch. Production system-level wins come from the
caller's prefetch lookahead (kevy-rt's parse loop) + cache-conscious K type
(SmallBytes), neither of which the standalone bench exercises.

## Reproducer

```bash
# bench vs std::HashMap (FxBuildHasher baseline)
cargo run --release -p kevy-map --example bench_vs_std

# budget gate (regression detector)
cargo test --release -p kevy-map --test perf_gate
```

## Bench numbers — mac aarch64 (dev box, loaded host)

Recorded 2026-05-26, M-series Mac under typical dev load (`uptime` ~ 3-5).
`Vec<u8>` keys formatted as `key:NNNNNNNN`, value `u64`. Each row median of
30 samples × 500 inner.

| Op          | n      | KevyMap ns/op | std+Fx ns/op | std/kevy ratio |
|-------------|--------|---------------|--------------|----------------|
| insert      |    256 |            19 |           16 |          0.84× |
| get-hit     |    256 |             4 |            3 |          0.75× |
| insert      |  4 096 |            23 |           20 |          0.87× |
| get-hit     |  4 096 |             4 |            4 |          1.00× |
| insert      | 65 536 |            29 |           25 |          0.86× |
| get-hit     | 65 536 |             8 |            7 |          0.88× |

**Honest reading**: standalone KevyMap is **0.75-1.00×** of std::HashMap +
FxBuildHasher on mac aarch64 — i.e., parity-to-slightly-slower. The
production wins (10M-key GET +21% vs FxHashMap baseline on lx64) come from
mechanisms *outside* the standalone bench:

1. **`Store::prefetch_for_key` called by the reactor's parse loop** (kevy-rt
   one-step lookahead). The bench `for k in &ks { km.get(k) }` does not
   prefetch ahead, so KevyMap's bucket-addr API isn't exercised — std
   HashMap doesn't have one and isn't penalized.
2. **`SmallBytes` keys + dense bucket layout at 10M+ keys** = one cache line
   per lookup. mac bench uses `Vec<u8>` keys (the std-HashMap shape) at
   modest n = 65k where everything fits in L2; the DRAM-bound 10M-key
   keyspace where bucket prefetch matters most is unrepresented.
3. **No load-factor pressure**: bench uses `with_capacity(n)` so the table
   never grows during measurement; production keyspaces hit the 7/8 LF
   regularly.

## Budget gates (perf_gate.rs)

Tests assert per-op median stays under conservative ceilings. Trip = the
regression is real (bigger than host noise band).

| Test                       | Ceiling     | Note |
|----------------------------|-------------|------|
| `insert_under_budget`      | 200 ns/op   | KEYS=1024, per-insert isolated |
| `get_hit_under_budget`     |  80 ns/op   | KEYS=1024, per-get isolated |
| `remove_under_budget`      | 250 ns/op   | combined insert+remove ceiling |

Current readings sit at ~20-50 ns/op for cache-hot single-thread paths;
the 200/80/250 ceilings absorb 4-10× headroom for loaded-host noise.

## Things this file does not yet cover

- aarch64 NEON group scan (hashbrown's win on mac comes partly from this;
  kevy-map's scalar metadata scan is the parity gap)
- lx64 x86_64 numbers (the production target; bench should also run there
  to lock in the comparison)
- prefetch-on / prefetch-off A/B (the system-level win attribution)
- memory bytes per entry vs std::HashMap

All TODO. The audit doc (AUDIT-2026-05-26.md) tracks them.
