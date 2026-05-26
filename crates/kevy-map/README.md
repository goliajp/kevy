# kevy-map

An **open-addressing hashtable** with Swiss-table-style SIMD probing and a
**bucket-addr API** that exposes the underlying slot pointer for prefetch —
pure Rust, zero dependencies.

Part of [kevy](https://crates.io/crates/kevy), the single-machine
Redis-compatible KV server. kevy needs (a) microsecond-budget hash lookups on
the hot path, and (b) a way to **prefetch the next bucket** while the current
one is still being compared. `kevy_map::KevyMap` exposes
`bucket_addr_for(key)` so the caller can issue a `__builtin_prefetch` on the
next key's bucket before reading the current one — that overlap is what
hides the 60–100 ns DRAM latency on the memory-wall hot path.

```rust
use kevy_map::KevyMap;

let mut m: KevyMap<u64, u64> = KevyMap::with_capacity(1024);
m.insert(7, 42);
assert_eq!(m.get(&7), Some(&42));
```

For prefetched batches:

```rust
# use kevy_map::KevyMap;
# let mut m: KevyMap<u64, u64> = KevyMap::with_capacity(1024);
# m.insert(7, 42);
let keys = [7u64, 8, 9];
for k in &keys {
    let addr = m.bucket_addr_for(k); // raw bucket pointer
    kevy_map::prefetch_t0(addr);     // hint NEXT read
    // …do work on the previous key here, hiding DRAM latency
    let _ = m.get(k);
}
```

## Status

Used in production inside kevy-store and kevy-rt; covered by ≥98% line coverage
and a perf-gate test (`tests/perf_gate.rs`).

## License

MIT OR Apache-2.0
