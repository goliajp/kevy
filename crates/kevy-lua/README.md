# kevy-lua

Lua scripting bridge for [kevy](https://crates.io/crates/kevy). Wraps the
pure-Rust [`luna-core`](https://crates.io/crates/luna-core) interpreter
into the Redis `EVAL` / `EVALSHA` / `SCRIPT` command surface, defaulting
to Lua 5.1 (for Redis ecosystem compatibility) with per-script opt-in
to 5.2 / 5.3 / 5.4 / 5.5 via the shebang `#!lua version=N`.

## Status

**v1.27 functional complete.** Every Redis Lua command works end-to-end
against a real kevy server: `EVAL`, `EVALSHA`, `EVAL_RO`,
`EVALSHA_RO`, `SCRIPT LOAD/EXISTS/FLUSH`. The canonical Redis-Lua
ecosystem scripts (Redlock unlock/extend, atomic incr-or-init, etc.)
run byte-for-byte from the `/tmp/lua-ecosystem-survey/` corpus. Read
the full reference at [`docs/lua.md`](../../docs/lua.md).

Known v1.28 follow-ups: `cjson` / `cmsgpack`, `FUNCTION LOAD`, LDB
debugger, full TOML config plumbing. None of these block ecosystem
compat for the standard Redis Lua API.

## Why default to Lua 5.1?

Every real-world Redis Lua script in the ecosystem — BullMQ's command
suite, Redlock, rate limiters, anything copied from Redis docs — is
written against PUC Lua 5.1.5 (the version Redis itself ships). A
multi-dialect kevy that defaulted to 5.5 would break drop-in
compatibility on day one.

Instead, kevy-lua keeps 5.1 as the default and lets new scripts opt
into newer dialects with a single shebang line:

```lua
#!lua version=5.3
-- now I can use integer math, goto, generic-for closures.
local count = redis.call("INCR", KEYS[1])  -- typed integer in 5.3+
```

This pattern extends Redis 7.0's existing `#!lua name=...` Functions
shebang syntax with a new `version=` key. Redis-ecosystem scripts that
don't use the shebang continue to run on 5.1 exactly as they would on
real Redis.

## Why luna-core?

luna is GOLIA's own pure-Rust Lua runtime (910 tests / 0 failures /
123 PUC official test files passing across all five dialects). The
v1.1 split publishes `luna-core` as the 0-dep interpreter and
`luna-jit` as the Cranelift-JIT-equipped variant. kevy uses
`luna-core` exclusively — adding it pulls **0 new transitive
third-party crates** into the kevy dependency tree, preserving kevy's
0-dep workspace rule.

## License

Apache-2.0 OR MIT, at your option.
