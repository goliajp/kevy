# Memory budgets — kevy-bytes

Per-op heap usage and stack footprint, with the source code path that
guarantees each number. Drift here is a published-contract regression.

## Stack layout

`size_of::<SmallBytes>() == 24` and `align_of::<SmallBytes>() == align_of::<usize>()`.
Asserted at compile time in `src/lib.rs`:

```rust
const _: () = {
    assert!(core::mem::size_of::<SmallBytes>() == 24);
    assert!(core::mem::align_of::<SmallBytes>() == core::mem::align_of::<usize>());
};
```

The `tests/perf_gate.rs::size_and_align_pinned` test re-asserts at runtime
for cross-compile validation.

## Per-op heap allocations

| Operation                | Payload ≤ 22 B | Payload > 22 B | Source |
|--------------------------|----------------|----------------|--------|
| `SmallBytes::from_slice` | **0 alloc**    | 1 alloc        | inline writes into the union; long path is `Box<[u8]>` |
| `SmallBytes::from_vec`   | **0 alloc**    | 0 alloc (reuses the Vec's buffer) | `Vec::into_boxed_slice` |
| `clone()`                | **0 alloc**    | 1 alloc (deep copy)               | heap path duplicates the buffer |
| `as_slice()` / `len()` / `is_empty()` | **0 alloc** | **0 alloc**          | borrows only |
| `to_vec()`               | 1 alloc        | 1 alloc        | unconditional copy out |
| `drop()`                 | **0 alloc**    | 1 dealloc      | heap path frees the buffer |

## Inline-threshold contract

The threshold is **22 bytes** (24 union bytes minus 1 length+tag byte at the
high end minus the discriminator placement). It is not configurable — that
keeps the layout const-assertable and the hot-path branch predictable.

Caller's invariant: if your domain has p99 payload ≤ 22 B, you pay zero
allocations on the common path. The heap path is a 1-alloc `Vec<u8>`-parity
fallback.

## Heap-path constant overhead

A heap-stored `SmallBytes` adds **0 bytes** of metadata beyond the
`(ptr, len)` pair already in the 24-byte union — no separate capacity field
(it's not growable in place).

## Verifying live

```bash
cargo run --release -p kevy-bytes --example bench_sso
cargo test --release -p kevy-bytes --test perf_gate
```

The bench prints per-op ns; the gate test fails the build if budgets regress.

## Caveats

- "0 alloc" is verified by code review + path coverage; an alloc-count test
  (custom `GlobalAlloc` that counts) is listed as a future hardening in
  AUDIT-2026-05-26.md.
- "1 alloc" on the heap path could become 0 in the future if `from_slice`
  is rewritten to write into an inline `Box::new_uninit_slice`-then-`assume_init`
  cycle — currently 1 alloc.
