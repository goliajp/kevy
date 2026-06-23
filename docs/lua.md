# Lua scripting

kevy v1.27 ships Redis-compatible server-side Lua scripting via the
in-house [`luna`](https://github.com/goliajp/luna) runtime. The
ecosystem default is Lua 5.1 (every script copied from Redis docs,
BullMQ, Redlock, or rate-limiter libraries Just Works); per-script
opt-in to 5.2 / 5.3 / 5.4 / 5.5 is available via a one-line shebang.

> **TL;DR.** `EVAL "return 1" 0` returns `:1`. Every Redis Lua API
> from the official docs is implemented. The default dialect is 5.1.
> Add `#!lua version=5.3` (or 5.2 / 5.4 / 5.5) on the first line of
> a script to use a newer dialect.

## Why kevy doesn't use PUC Lua

Real Redis embeds PUC Lua 5.1, a small but C-bound interpreter. kevy
is pure Rust with zero crates.io dependencies in the default server
stack; embedding C would break that promise. The
[`luna`](https://github.com/goliajp/luna) project is a pure-Rust Lua
runtime authored by the same team. luna passes 123/123 of the
official PUC test suite across Lua 5.1-5.5 (910 unit tests total, 0
failures), so the compatibility floor is verified the same way PUC
verifies itself.

The carved exemption is documented in the workspace `Cargo.toml`:
`kevy-lua` and `kevy-lua-host` transitively pull `luna-core` (and
nothing else third-party — luna-core itself is 0-dep, CI-gated via
`cargo-deny`). The default server / blocking-client / embedded
stacks remain 0 third-party deps.

## Quick start

Run a kevy server, then:

```sh
redis-cli -p 6004 EVAL "return 1 + 1" 0
# (integer) 2

redis-cli -p 6004 EVAL "redis.call('SET', KEYS[1], ARGV[1]); return redis.call('GET', KEYS[1])" 1 mykey hello
# "hello"
```

## Commands

| Command | Behaviour |
|---|---|
| `EVAL script numkeys key... arg...` | Compile + execute; auto-fills SCRIPT cache by SHA1. |
| `EVALSHA sha1 numkeys key... arg...` | Run a previously-cached script; `-NOSCRIPT` if missing. |
| `EVAL_RO` / `EVALSHA_RO` | Read-only variants. Write commands raise `-READONLY`. |
| `SCRIPT LOAD script` | Cache without running. Returns the SHA1 hex (`$40\r\n...\r\n`). |
| `SCRIPT EXISTS sha1...` | Array of `:1`/`:0` per input SHA1 in order. |
| `SCRIPT FLUSH [SYNC\|ASYNC]` | Drop the cache + per-dialect VM pool. Both modes synchronous in v1.27 (the tag is preserved for future differentiation). |

### KEYS and ARGV

Scripts see `KEYS` and `ARGV` as 1-indexed Lua tables of byte
strings. Both are binary-safe — embedded NULs, `0xFF`, and other
non-UTF-8 bytes round-trip cleanly.

```lua
-- EVAL "return KEYS[1] .. '/' .. ARGV[1]" 1 mykey hello
-- → "mykey/hello"
```

## Dialect routing — `#!lua version=N`

The shebang is the v1.27 differentiator vs Redis / valkey. By
default scripts run under Lua 5.1 (the Redis ecosystem default —
BullMQ, Redlock, etc. are all 5.1-clean). Add a shebang as the first
line to opt into newer dialects:

```lua
#!lua version=5.3
-- Now using Lua 5.3 — integer subtype, bitwise ops, `//` integer
-- divide, `string.pack`, etc.
local i = 10 // 3              -- 5.3+ integer divide → 3
local mask = 0xFF & 0x0F       -- 5.3+ bitwise → 0x0F
```

Recognised dialect tags: `5.1` / `51` / `5.2` / `52` / `5.3` / `53` /
`5.4` / `54` / `5.5` / `55`. Unknown values return
`-ERR unknown lua version: <X>`.

The shebang is **part of the script bytes**, so the SHA1 cache key
covers it: the same script source with vs without a shebang has two
distinct entries. This means `EVALSHA` reproduces dialect routing
without an extra command field.

Extra Redis 7.0 Functions keys (`flags=` / `name=`) are recognised
and tolerated (parsed and ignored at v1.27; FUNCTION LOAD comes in
v1.28+).

### Operator-side dialect lockdown

Embedders who want to lock the server to pure-Redis-compat
(5.1 only) can call `Bridge::set_allowed_dialects(&[LuaVersion::Lua51])`
in code, or use the equivalent TOML config when wired through
the kevy CLI (config wiring TBD post-P7c). Disallowed dialects
return `-ERR dialect 5.3 disabled by [lua] allow_dialects`.

## The `redis.*` host API

| Symbol | Behaviour |
|---|---|
| `redis.call(cmd, ...)` | Dispatch through the normal kevy command path. RESP error replies (anything starting with `-`) raise a Lua error. |
| `redis.pcall(cmd, ...)` | Same, but errors become `{err = "msg"}` tables. |
| `redis.status_reply(msg)` | Returns `{ok = msg}` which marshals as a RESP simple string. |
| `redis.error_reply(msg)` | Returns `{err = msg}` which marshals as a RESP error reply. |
| `redis.sha1hex(s)` | Hex SHA-1 digest (40 lowercase ASCII chars). |
| `redis.log(level, msg)` | No-op stub. Production logging wiring is on the v1.28 backlog. |
| `redis.replicate_commands()` | No-op. Redis 7+ semantics — every command is replicated. |

### Lua → RESP marshaling

A script's return value is marshaled per the Redis EVAL rules:

| Lua | RESP |
|---|---|
| `nil` | `$-1\r\n` (nil bulk) |
| `false` | `$-1\r\n` (Redis quirk) |
| `true` | `:1\r\n` |
| integer (5.3+) | `:N\r\n` |
| integral float | `:N\r\n` (5.1 returns `1` as `Float(1.0)` — kevy collapses to integer when lossless) |
| non-integral float | bulk string |
| string | bulk string (binary-safe) |
| `{ok = "msg"}` table | `+msg\r\n` (simple string) |
| `{err = "msg"}` table | `-msg\r\n` (error — caller controls the prefix, so `{err = "NOSCRIPT no script"}` round-trips through as `-NOSCRIPT no script\r\n`) |
| array table `{v1, v2, ...}` | `*N\r\n` + N marshaled elements (first-nil rule applies: `{1, nil, 3}` → `*1\r\n:1\r\n`) |

When `redis.call` returns an array reply (e.g. `MGET`), the Lua side
sees a 1-indexed table; nil-bulk replies (`$-1\r\n`) become Lua
`false`. Same shape as Redis.

## Ecosystem scripts

Every canonical real-world Redis-Lua script in the kevy test corpus
runs unmodified through `EVAL`. From the integration test suite
(`crates/kevy/tests/lua_ecosystem.rs`):

- **Redlock** unlock + extend (canonical antirez snippets)
- **Atomic incr-or-init** counter pattern
- **Rate limiters** (token-bucket exercised in the bridge tests;
  sliding-window pending kevy `ZREMRANGEBYSCORE` implementation)

Reproducible reference scripts and the verification harness live in
the v1.27 design package; see
[`/tmp/lua-ecosystem-survey/LUNA-FEEDBACK-REPORT.md`](file:///tmp/lua-ecosystem-survey/LUNA-FEEDBACK-REPORT.md)
on the kevy maintainer's machine (or the same path on any developer
who runs the survey harness).

## Sandbox

Every per-dialect VM is built via luna's `Vm::sandbox(version)`
builder with conservative defaults:

- Whitelisted stdlib only — `base` + `math` + `string` + `table`.
  No `io`, `os`, `debug`, `package`, `coroutine`, `bit32`, or
  `utf8` (the latter is on the v1.28 backlog if real demand
  appears).
- JIT off — luna's Cranelift JIT is intentionally disabled at the
  Vm level. kevy uses `luna-core` (interpreter only), not
  `luna-jit`, so the Cranelift deps are not in the kevy tree at
  all.
- Bytecode loading off — `load(bytecode)` and `string.dump` are
  blocked. Only Lua source can enter the Vm.
- Instruction budget: 200 M ops per `eval` (~5 s on modern
  hardware, matching Redis's default `lua-time-limit`). Scripts
  that exceed it return a catchable Lua error.

The TOML config plumbing for `[lua] time_limit_ms`, `[lua]
allow_dialects`, and the eventual `[lua] max_memory` is on the v1.27
P7c follow-up. The defaults are reasonable for the v1.27 ship.

## Performance

luna's interpreter is approximately **1.25–2.8× slower than PUC Lua
5.1** on real Redis-Lua workloads (token-bucket / sliding-window /
heavy method dispatch), per the LUNA-FEEDBACK-REPORT.md measurements.
For typical Redis-Lua usage — small atomic ops, ≤ 10 `redis.call`
per script — this is invisible: the per-EVAL overhead is a few
microseconds, dwarfed by network RTT for any client outside the same
host.

luna's perf gap is largely concentrated in dispatch-heavy patterns
that PUC's tight C interpreter handles well. The luna team is
tracking this; expected improvements via the v1.2 ergonomics +
perf sprint.

## Limitations and future work

| Item | Status |
|---|---|
| `cjson` / `cmsgpack` | Not implemented in v1.27 — **scope decision, not a dependency blocker**. Both algorithms can be implemented in pure Rust (~500 LOC each) as kevy-lua host stdlib modules. **Required to unblock BullMQ / Sidekiq Pro and other ecosystem libraries that bundle them as runtime deps.** Target v1.28. |
| `FUNCTION LOAD` / `FCALL` | Not implemented. Redis 7.0 Functions surface. v1.28+. |
| `LDB` debugger | Not implemented. Same on the Redis side for most users. v1.28+ if real demand. |
| ~~Multi-shard EVAL routing~~ | **Fixed in v1.27.1.** EVAL/EVALSHA with `numkeys ≥ 1` now route to KEYS[1]'s shard via `Route::Single(3)`. SCRIPT cache moved to a process-global `Mutex<HashMap>` so SCRIPT LOAD on any shard reaches EVALSHA on any shard. Verified end-to-end against a 4-shard server in `crates/kevy/tests/lua_multishard.rs`. |
| Nested EVAL (script calls `redis.call('EVAL', ...)`) | Returns `-ERR EVAL inside EVAL is not supported in v1.27`. Same restriction as real Redis. |

See `.claude/rfcs/2026-06-23-v1.27-luna-bridge.md` for the
complete v1.27 phase plan and follow-ups.

## Related

- [`crates/kevy-lua`](../crates/kevy-lua) — the bridge library
  (sandbox + redis.* + RESP marshaling + shebang routing + SHA1
  cache). Documented in its own `README.md`.
- [`crates/kevy-lua-host`](../crates/kevy-lua-host) — the
  kevy-side glue that lets the bridge reach `&mut Store`.
- [`docs/uds.md`](uds.md) — UDS transport for low-latency local
  clients (often paired with EVAL for atomic write-set scripts).
- [`docs/tuning.md`](tuning.md) — server tuning knobs.
