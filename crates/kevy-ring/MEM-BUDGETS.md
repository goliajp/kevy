# Memory budgets — kevy-ring

The ring owns one contiguous `Vec<MaybeUninit<T>>` of `cap` slots plus a
cache-line-padded `head` + `tail` pair. No per-message allocation.

## Per-op heap allocations

| Operation                    | Allocations | Source |
|------------------------------|-------------|--------|
| `ring::<T>(cap)`             | 1           | single `Vec<MaybeUninit<T>>` sized to `ceil_pow2(cap)`. Splits into a `Producer<T>` + `Consumer<T>` over the same buffer. |
| `Producer::push(v)`          | **0**       | writes into a slot via `ptr::write`. |
| `Consumer::pop()`            | **0**       | reads via `ptr::read`. |
| `Drop` of half-empty ring    | dealloc     | iterates remaining `T`s and drops them, then deallocates the buffer. |

## Stack footprint

`size_of::<Producer<T>>()` = `Arc<RingState<T>>` = **8 B** (one pointer).
Same for `Consumer<T>`. The shared `RingState<T>` is one allocation
(buffer + atomics + padding).

`RingState<T>` layout (64-bit, 64-B cache line):

```
+--------------------------+   offset 0
| buffer: Box<[MaybeUninit<T>]> |  (16 B)
+--------------------------+
|  …padding to 64…         |
+--------------------------+   offset 64
| tail: AtomicUsize         |  (writer-side hot)
+--------------------------+
|  …padding to next line…  |
+--------------------------+   offset 128
| head: AtomicUsize         |  (reader-side hot)
+--------------------------+
```

Producer and consumer indices live on **separate cache lines** — that's
the false-sharing avoidance the cross-core throughput depends on.

## Verifying live

```bash
cargo run --release -p kevy-ring --example bench_ring
cargo test --release -p kevy-ring --test perf_gate
```

## Caveats

- Capacity is rounded UP to a power of two — caller asks for 100 slots,
  gets 128. The wasted slots are filled, not held back.
- `MaybeUninit<T>` per slot — slots that have never been pushed contain
  uninitialized memory; consumed slots are also uninitialized (we drop
  the `T` on pop). Bounds-checking is index-based, not init-flag based.
