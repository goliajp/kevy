# kevy-bytes

A 24-byte small-byte-string (`SmallBytes`) with inline SSO — pure Rust, zero
dependencies, alloc-free for payloads ≤ 22 bytes.

Part of [kevy](https://crates.io/crates/kevy), a single-machine,
Redis-compatible key–value server, but designed to stand alone wherever you
want short bytes stored without an extra pointer chase.

- **Inline up to 22 bytes** — payload + tag-byte stored directly inside the
  24-byte union; no allocation, no heap pointer.
- **Heap path for longer** — same external API; switches to a length-prefixed
  heap buffer transparently.
- **24-byte size & `usize` alignment** are const-asserted (won't compile if
  violated).
- **Zero dependencies.** Only `kevy-hash` (path dep, also pure Rust) for the
  optional `Hash` impl.

```rust
use kevy_bytes::SmallBytes;

let inline = SmallBytes::from_slice(b"redis");   // 5 ≤ 22 → no alloc
let heap   = SmallBytes::from_slice(&[0u8; 64]); // 64 > 22 → one alloc
assert_eq!(inline.as_slice(), b"redis");
assert_eq!(heap.len(), 64);
```

## Safety

The SSO union needs `unsafe` for the tag-byte read; every `unsafe` block in
the crate has a `SAFETY:` comment justifying the union discriminant. It is
LE-only (compile-time guarded by `compile_error!` on BE targets).

## License

MIT OR Apache-2.0
