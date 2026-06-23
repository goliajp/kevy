# kevy-lua

Lua scripting bridge for [kevy](https://crates.io/crates/kevy). Wraps the
pure-Rust [`luna-core`](https://crates.io/crates/luna-core) interpreter
into the Redis `EVAL` / `EVALSHA` / `SCRIPT` command surface, defaulting
to Lua 5.1 (for Redis ecosystem compatibility) with per-script opt-in
to 5.2 / 5.3 / 5.4 / 5.5 via the shebang `#!lua version=N`.

## Status

**Pre-1.0 — v1.27 development in progress.** Skeleton only at P0; the
public API surface is stubbed and EVAL returns a `-ERR kevy-lua P0
stub` placeholder reply. Full plumbing lands across P1 → P9. See
`.claude/rfcs/2026-06-23-v1.27-luna-bridge.md` in the kevy repo for the
phase plan.

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
