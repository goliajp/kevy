# kevy-hash performance budgets

`KevyHash` trait collapses the std `Hasher` state-machine to a single
inlinable call; `FxHasher` is the `Hasher` impl callers can plug into
`std::HashMap` directly.

## Reproducer

```bash
cargo run --release -p kevy-hash --example bench_hash
cargo test --release -p kevy-hash --test perf_gate
```

## Bench numbers — mac aarch64 (loaded host)

Recorded 2026-05-26. Median of 60 samples × 200k inner. Sub-1ns rows are
below the bench harness's `Instant::now()` resolution floor — kevy-hash
is fast enough that single-op timing flatlines at 0 ns/op.

| Input         | KevyHash | FxHasher | SipHash (std) | SipHash / KevyHash |
|---------------|----------|----------|---------------|--------------------|
| bytes[8]      |    < 1ns |    < 1ns |          3ns  |              >3×   |
| bytes[16]     |    1 ns  |    1 ns  |          5ns  |               5×   |
| bytes[24]     |    1 ns  |    1 ns  |          6ns  |               6×   |
| bytes[32]     |    1 ns  |    1 ns  |          7ns  |               7×   |
| bytes[64]     |    3 ns  |    2 ns  |         13ns  |             4.3×   |
| bytes[128]    |    7 ns  |    7 ns  |         26ns  |             3.7×   |
| u64           |    < 1ns |    < 1ns |          3ns  |              >3×   |

→ Headline: **3.7-7× faster than std SipHash** on the byte-string +
integer key shapes kevy actually uses. The win is `Fx absorb + fmix64`
having ~6-10 ALU ops total vs SipHash's keyed rounds.

## Budget gates (perf_gate.rs)

| Test                            | Ceiling | What it gates |
|---------------------------------|---------|---------------|
| `kevy_hash_bytes_under_budget`  | 50 ns   | 16-byte byte-string via `KevyHash::kevy_hash` |
| `kevy_hash_u64_under_budget`    | 20 ns   | u64 via `KevyHash::kevy_hash` |
| `fxhasher_bytes_under_budget`   | 70 ns   | 16-byte byte-string via `FxHasher::write + finish` |

Ceilings give ~5-50× headroom over the measured cache-hot numbers — they
catch real regressions without firing on host-noise.

## Avalanche guard

Not a perf budget but a correctness budget: `tests::no_catastrophic_clustering_on_low_entropy_keys`
asserts that on 4096 keys of shape `"key:NNNN"` neither the low bits (bucket
index) nor the top 7 bits (hashbrown control byte) cluster beyond ~4× the
mean. If `fmix64` is ever removed or weakened, this test trips loudly.

## Things this file does not yet cover

- aarch64-vs-x86_64 perf comparison (bench should also run on lx64).
- Worst-case adversarial input behavior (not relevant: this hasher is
  explicitly NOT DoS-resistant; single-trust-domain is the documented
  threat model).
