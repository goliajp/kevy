# kevy-lua-host

kevy-side glue that lets [`kevy-lua`](../kevy-lua)'s `Bridge` reach
the per-shard mutable state (`Store`, etc.) without crates.io
thread-local helpers. Owns one tiny scoped-pointer indirection so
the bridge's `Fn(&[&[u8]], bool) -> Vec<u8> + 'static` dispatch
closure can borrow `&mut T` from the host call frame.

The `unsafe` footprint is one `unsafe { &mut *p }` inside
`with_current`, audited per commit. Single-threaded per shard means
no aliasing or synchronisation concerns. See the crate-level docs in
`src/lib.rs` for the full safety contract.

## License

MIT OR Apache-2.0.
