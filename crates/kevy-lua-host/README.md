# kevy-lua-host

The kevy-server-side glue between the
[`kevy-lua`](https://crates.io/crates/kevy-lua) bridge and the kevy
server's command dispatch path. Owns the per-shard Lua bridge plus
the scoped raw-pointer indirection that lets the bridge's dispatch
closure reach `&mut Store` without a thread-local helper crate.

The `unsafe` footprint is one `unsafe { &mut *p }` inside
`with_current`, audited per commit. Single-threaded per shard means
there are no aliasing or synchronisation concerns. The crate-level
docs in `src/lib.rs` carry the full safety contract.

## Audience

Internal infrastructure for the kevy server. End users invoke Lua
via `redis-cli EVAL` and friends — see
[`docs/lua.md`](https://github.com/goliajp/kevy/blob/develop/docs/lua.md).

## Dependencies

Third carved exemption to the workspace's pure-Rust 0-dependency
rule. Transitively pulls one `crates.io` crate: the same `luna-core`
used by [`kevy-lua`](https://crates.io/crates/kevy-lua).

## License

MIT OR Apache-2.0, at your option.
