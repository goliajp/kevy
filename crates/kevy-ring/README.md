# kevy-ring

A lock-free, bounded **single-producer / single-consumer (SPSC)** ring buffer —
pure Rust, zero dependencies.

One producer pushes, one consumer pops, with no locks and **no per-message
allocation**: a fixed power-of-two slot array plus two cache-line-padded
cursors. It is the cross-core transport primitive for [`kevy-rt`]'s
shared-nothing, thread-per-core runtime (the Seastar/Scylla model) — each ordered
pair of cores gets its own ring, so a hot reactor never contends a lock on the
cross-core hop.

The SPSC contract is enforced **by the type system**: `push`/`pop` take
`&mut self` on distinct owned `Producer`/`Consumer` halves, so the compiler
guarantees at most one thread pushes and one pops. That keeps the synchronization
to a single `Release`/`Acquire` pair per operation.

```rust
let (mut tx, mut rx) = kevy_ring::ring::<u32>(1024);
tx.push(1).unwrap();
assert_eq!(rx.pop(), Some(1));
```

`push` hands the value back as `Err(val)` when the ring is full, so the caller
chooses the back-off policy (spin, yield, or — in `kevy-rt` — drain its own inbox
and retry, which is what avoids deadlock in the all-to-all core mesh).

## Safety

The lock-free buffer needs `UnsafeCell` + atomics, so this crate is not
`#![forbid(unsafe_code)]`. Every `unsafe` block documents the SPSC invariant it
relies on. It is still pure Rust with no C and no FFI, per the kevy project's
pure-Rust principle.

Part of the [kevy](https://crates.io/crates/kevy) key–value server.

## License

MIT OR Apache-2.0
