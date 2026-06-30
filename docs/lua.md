# Lua scripting

Server-side Lua scripting in kevy: how to run atomic scripts with `EVAL` / `EVALSHA`, what bindings exist, and how to opt into Lua dialects newer than 5.1.

## When you need this

Reach for Lua scripting when you want to:

- Execute a small multi-command sequence atomically against one key (check-then-set, conditional counters, distributed locks).
- Push read-modify-write logic that a single command can't express into the server, eliminating round trips.
- Run a script published by an ecosystem library (BullMQ, Sidekiq, Bee Queue, Redlock, sliding-window rate limiters) unchanged.

For ordinary single-command access or for transactions across many keys with explicit optimistic locking, use plain commands or `MULTI` / `EXEC` instead.

## Core idea

`EVAL` ships a Lua source string to the server, compiles it, and runs it on the shard that owns `KEYS[1]`. While the script runs, no other command on that shard interleaves with it — the whole script is one atomic unit. Inside the script, `redis.call("CMD", ...)` dispatches back through the normal command path, `KEYS` and `ARGV` give 1-indexed binary-safe access to the inputs, and the script's return value is marshaled to RESP. Loaded scripts are cached by SHA1, so `SCRIPT LOAD` + `EVALSHA` lets clients send the hash instead of the body on every call. The Lua runtime is [luna](https://github.com/goliajp/luna), a pure-Rust 5.1 – 5.5 interpreter. Default dialect is Lua 5.1 (matching what every Redis-ecosystem script expects); a one-line shebang opts a single script into 5.2, 5.3, 5.4, or 5.5.

## Worked example: capped counter

The script bumps a counter by 1, but only if the new value would stay at or below a cap. Returns the new value, or `nil` if the cap would be exceeded.

```lua
-- KEYS[1] = counter key
-- ARGV[1] = cap (integer)
local cur = tonumber(redis.call("GET", KEYS[1]) or "0")
local cap = tonumber(ARGV[1])
if cur + 1 > cap then
  return nil
end
return redis.call("INCR", KEYS[1])
```

### Inline via `EVAL`

```sh
redis-cli -p 6004 EVAL \
  "local cur = tonumber(redis.call('GET', KEYS[1]) or '0')
   local cap = tonumber(ARGV[1])
   if cur + 1 > cap then return nil end
   return redis.call('INCR', KEYS[1])" \
  1 quota:user:42 5
# (integer) 1
# … four more calls …
# (integer) 5
# next call:
# (nil)
```

### Cached via `SCRIPT LOAD` + `EVALSHA`

```sh
SHA=$(redis-cli -p 6004 SCRIPT LOAD \
  "local cur = tonumber(redis.call('GET', KEYS[1]) or '0')
   local cap = tonumber(ARGV[1])
   if cur + 1 > cap then return nil end
   return redis.call('INCR', KEYS[1])")
echo "$SHA"
# e.g. 7c3e0a9b1d4f...

redis-cli -p 6004 EVALSHA "$SHA" 1 quota:user:42 5
# (integer) 1

redis-cli -p 6004 SCRIPT EXISTS "$SHA"
# 1) (integer) 1
```

If a client sends `EVALSHA` for a hash the server has never seen (cold start, `SCRIPT FLUSH`), kevy returns `-NOSCRIPT` and the client should fall back to `EVAL`. The script body cached by SHA1 includes any shebang line, so a 5.1 and a 5.4 copy of the same source are distinct cache entries.

### Lua 5.4 dialect via shebang

```lua
#!lua version=5.4
-- ARGV[1] = max retries before giving up
local tries = tonumber(ARGV[1])
local i = 0
::again::
i = i + 1
local ok = redis.call("SET", KEYS[1], "owned", "NX", "PX", 3000)
if type(ok) == "table" and ok.ok == "OK" then
  return i
end
if i < tries then goto again end
return redis.error_reply("LOCK_FAILED")
```

`goto` / labels and integer-typed arithmetic are 5.3+ features; the shebang routes this one script to the 5.4 VM pool while other scripts keep running on 5.1.

## Bindings

| Symbol | Behaviour |
|---|---|
| `redis.call(cmd, ...)` | Dispatch a kevy command. RESP errors raise a Lua error and abort the script unless caught with `pcall`. |
| `redis.pcall(cmd, ...)` | Same dispatch, but RESP errors return as `{err = "msg"}` instead of raising. |
| `redis.error_reply(msg)` | Build `{err = msg}`; when returned from the script it marshals to `-msg\r\n`. |
| `redis.status_reply(msg)` | Build `{ok = msg}`; marshals to `+msg\r\n` (simple string). |
| `redis.sha1hex(s)` | 40-char lowercase hex SHA-1 of the input bytes. |
| `redis.replicate_commands()` | No-op. Every script already replicates atomically as a unit. |
| `KEYS` | 1-indexed table of `numkeys` byte strings declared in the `EVAL` call. Binary-safe. |
| `ARGV` | 1-indexed table of the remaining arguments. Binary-safe. |
| `cjson.encode(v)` / `cjson.decode(s)` | Pure-Rust JSON codec. Same surface as the Redis `cjson` library. |
| `cmsgpack.pack(v)` / `cmsgpack.unpack(s)` | Pure-Rust MessagePack codec. Same surface as the Redis `cmsgpack` library. |

Lua return values marshal to RESP using the standard Redis rules: `nil` and `false` become nil-bulk, `true` becomes `:1`, integers and lossless-integer floats become integer replies, other floats and strings become bulk strings, `{ok=...}` becomes a simple string, `{err=...}` becomes an error reply, and a plain array becomes a multi-bulk reply (with the first-nil truncation rule).

## Dialect selection

| First line of script | Dialect used |
|---|---|
| (no shebang) | Lua 5.1 (default; what BullMQ, Sidekiq, Redlock, and most published Redis-Lua snippets assume) |
| `#!lua version=5.1` (or `51`) | Lua 5.1, explicit |
| `#!lua version=5.2` (or `52`) | Lua 5.2 — `goto`, `_ENV`, ephemeron tables |
| `#!lua version=5.3` (or `53`) | Lua 5.3 — integer subtype, bitwise operators, `//`, `string.pack` / `string.unpack` |
| `#!lua version=5.4` (or `54`) | Lua 5.4 — to-be-closed variables, integer `for` semantics, new bitwise corners |
| `#!lua version=5.5` (or `55`) | Lua 5.5 — latest published dialect |
| `#!lua version=<other>` | `-ERR unknown lua version: <X>` |

Redis 7.0 Functions metadata (`flags=`, `name=`) on the shebang line is parsed and ignored, so scripts written for the Functions surface load cleanly under `EVAL`.

## Trade-offs and limits

- **No filesystem, network, or OS access.** `io`, `os`, `package`, `debug`, and `coroutine` are not loaded. Scripts cannot open files, make sockets, spawn processes, or read environment variables.
- **No bytecode loading.** `load(bytecode)` and `string.dump` are blocked. Only Lua source can enter the VM, which closes the bytecode-verifier escape route that has historically broken Lua sandboxes.
- **Whitelisted standard library.** `base`, `math`, `string`, `table`, `cjson`, and `cmsgpack` are available. Other standard modules are absent.
- **Per-script time budget.** Each `EVAL` runs under an instruction budget of roughly 200 M ops (about 5 s of CPU on modern hardware, matching Redis's default `lua-time-limit`). Exceeding it returns a catchable Lua error and aborts the script.
- **Per-script memory budget.** Each script runs in a fresh interpreter state seeded from a per-dialect VM pool; tables and strings created during the call are reclaimed when it returns. There is no shared mutable Lua state between calls — use kevy keys to persist anything.
- **No nested `EVAL`.** A script calling `redis.call("EVAL", ...)` returns an error, matching Redis behaviour.
- **One shard per script.** All `KEYS` must hash to the same slot. Scripts that touch multiple keys across shards get `-CROSSSLOT` at dispatch.
- **JIT off by design.** The interpreter is used directly; the Cranelift JIT in luna is not linked into the kevy server, keeping the dependency surface minimal and avoiding JIT-time pauses.

## FAQ

**Will my existing Redis Lua script run unmodified?**
If it targets Lua 5.1 (the Redis default) and only uses `redis.call` / `redis.pcall` / `redis.error_reply` / `redis.status_reply` / `KEYS` / `ARGV` / `cjson` / `cmsgpack`, yes. If it relies on the debug library, the OS library, or loading precompiled bytecode, no — those are sandboxed out.

**How do I run a script across keys on different shards?**
You can't — atomicity is the whole point. Split the work into per-shard scripts and coordinate at the client, or design the key layout so the relevant keys share a hash tag (`{user:42}:quota` and `{user:42}:counter` route to the same shard).

**`EVALSHA` returns `-NOSCRIPT`. What do I do?**
Re-send the same body via `EVAL`. The server caches it again and answers the call. Most client libraries handle this fallback automatically. `SCRIPT FLUSH` and process restarts both reset the cache.

**Can a script call `BLPOP` or other blocking commands?**
No. Blocking commands inside `EVAL` would defeat the atomicity contract — the shard would either freeze waiting for itself or have to interleave other work. Blocking commands return an error when dispatched from a script.

**Why Lua 5.1 by default when newer versions exist?**
Every script published in the Redis ecosystem assumes 5.1. Defaulting to 5.1 means those scripts copy-paste in without surprises around integer subtypes, bitwise operators, or `goto`. Opting individual scripts into a newer dialect via the shebang is a one-line change when you actively want the newer features.
