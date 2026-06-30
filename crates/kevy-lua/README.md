# kevy-lua

The Lua scripting bridge for kevy. Wraps the in-house pure-Rust
[`luna-core`](https://crates.io/crates/luna-core) interpreter into
the Redis `EVAL` / `EVALSHA` / `SCRIPT` command surface. Default
dialect is Lua 5.1 (for Redis ecosystem compatibility); per-script
opt-in to 5.2 / 5.3 / 5.4 / 5.5 via the shebang `#!lua version=N`.
Includes pure-Rust `cmsgpack` and `cjson` standard libraries.

## Why Lua 5.1 by default

Every real-world Redis Lua script in the ecosystem — BullMQ's
command suite, Redlock, rate limiters, anything copied from the
Redis docs — is written against PUC Lua 5.1.5 (the version Redis
itself ships). A multi-dialect runtime that defaulted to 5.5 would
break drop-in compatibility on day one.

```lua
#!lua version=5.3
-- now I can use integer math, goto, generic-for closures.
local count = redis.call("INCR", KEYS[1])  -- typed integer in 5.3+
```

This pattern extends Redis 7.0's `#!lua name=...` Functions shebang
syntax with a new `version=` key. Scripts that don't carry a shebang
continue to run on 5.1 exactly as they would on real Redis.

## Audience

The bridge is consumed by the kevy server through the
[`kevy-lua-host`](https://crates.io/crates/kevy-lua-host) glue
crate. End users invoke Lua via `redis-cli EVAL` or the equivalent in
any Redis client library — see
[`docs/lua.md`](https://github.com/goliajp/kevy/blob/develop/docs/lua.md).

## Dependencies

This crate is one of three carved exemptions to the workspace's
pure-Rust 0-dependency rule. It depends on `luna-core` — the
in-house pure-Rust Lua runtime, itself zero-dependency on the `luna`
side. The net effect on the kevy server's dependency graph is one
transitive crate (`luna-core`).

## License

MIT OR Apache-2.0, at your option.
