# kevy-map

An **open-addressing hashtable** with Swiss-table-style SIMD probing and a
**bucket-prefetch hint API** — pure Rust, zero dependencies.

Part of [kevy](https://crates.io/crates/kevy), the single-machine
Redis-compatible KV server. kevy needs (a) microsecond-budget hash lookups
on the hot path, and (b) a way to **prefetch the next bucket** while the
current one is still being compared. `KevyMap::prefetch_for_hash(hash)`
issues a `prefetcht0` (x86_64) / `prfm pldl1keep` (aarch64) for the bucket
the probe would land on; the command-batch driver in `kevy-rt` calls this
for command `N+1` while finishing command `N`, hiding 60–100 ns of DRAM
latency on the memory-wall hot path.

```rust
use kevy_map::KevyMap;
use kevy_hash::KevyHash;

let mut m: KevyMap<u64, u64> = KevyMap::with_capacity(1024);
m.insert(7, 42);
assert_eq!(m.get(&7), Some(&42));
```

Prefetched batch lookup:

```rust
# use kevy_map::KevyMap;
# use kevy_hash::KevyHash;
# let mut m: KevyMap<u64, u64> = KevyMap::with_capacity(1024);
# m.insert(7, 42);
let keys = [7u64, 8, 9];
for window in keys.windows(2) {
    let next_hash = window[1].kevy_hash();
    m.prefetch_for_hash(next_hash); // hint NEXT bucket line into L1
    let _ = m.get(&window[0]);      // …do work on the current key
}
```

The `KevyHash` trait abstracts the hasher; `kevy-hash`'s `FxHasher + fmix64`
implements it for the kevy hot path. Other hashers can plug in.

## Status

Used inside `kevy-store` and `kevy-rt` for the hot-path lookup. Covered
by ≥98% line coverage and a perf-gate test (`tests/perf_gate.rs`).

## License

MIT OR Apache-2.0
