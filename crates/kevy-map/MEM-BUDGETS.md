# Memory budgets — kevy-map

`KevyMap<K, V>` is one contiguous allocation: `[Bucket<K, V>; cap]` where
`cap` is a power of two. There is no auxiliary per-bucket metadata; the
control byte lives inside `Bucket` for cache locality.

## Per-op heap allocations

| Operation                          | Allocations | Source |
|------------------------------------|-------------|--------|
| `with_capacity(n)`                 | 1           | single buffer of `ceil_pow2(n / 7 × 8)` buckets. |
| `insert(k, v)` (no resize)         | 0           | in-place writes. |
| `insert(k, v)` (triggers resize)   | 1 + 1 dealloc | new buffer at 2× cap; old freed at end. |
| `get(&k)` / `len()` / `is_empty()` / `iter()` | 0 | borrowing only. |
| `remove(&k)`                       | 0           | tombstone or shifted-deletion in-place. |
| `drop()`                           | 1 dealloc   | frees the single backing buffer. |

## Stack footprint

`size_of::<KevyMap<K, V>>()` = `ptr (8) + len (8) + cap (8) + tombstones (8)`
= **32 bytes** on 64-bit (excluding the K + V it owns through the heap buffer).

## Per-bucket overhead

`Bucket<K, V>` is `{ ctrl: u8, key: K, value: V }`, packed with the
alignment of `(K, V)`. Tombstones / vacant are tracked via reserved
control-byte values (`0xFF` for vacant, `0x80` for tombstone), so there is
no separate state vector — the control byte is one byte per bucket.

| K              | V    | Bucket size |
|----------------|------|------------:|
| `u64`          | `u64`| 24 B (1+8+8 + 7 padding) |
| `SmallBytes`(24) | `u64` | 32 B (1+24+8 padding+align) |
| `Vec<u8>` (24) | `u64`| 32 B |

At 7/8 LF the **bytes-per-entry** is `bucket_size × 8 / 7`. E.g. for
`(SmallBytes, u64)`: 32 × 8 / 7 ≈ 36.6 B/entry. For 10M keys that's
~366 MB of bucket array — within the kevy mem budget.

## Verifying live

```bash
cargo run --release -p kevy-map --example bench_vs_std
cargo test --release -p kevy-map --test perf_gate
```

## Caveats

- Resize doubles the buffer — peak resident during a resize is 3× the
  pre-resize size for a brief window (old + new + iter scratch). This is
  the standard hashtable amortization, not unique to kevy-map.
- Tombstone count is tracked; on excessive tombstone fraction the map
  re-inserts in place rather than reallocating (no allocation, but a full
  scan).
