# Error reply catalog

> v1.36 — every wire-level error kevy emits, with its trigger condition + recovery action. Treated as part of the user-facing contract; commands that change error semantics must update this catalog.

Errors are emitted as RESP simple-error strings: `-<ERR-CLASS> <message>\r\n`. The first space-separated token is the **class** (`ERR`, `WRONGTYPE`, `MOVED`, etc.). Catalog is grouped by class.

## `-ERR` (generic)

| Trigger | Recovery |
|---------|----------|
| `unknown command '<cmd>'` | Wrong / unsupported command name. Check kevy's command coverage in README. |
| `wrong number of arguments for '<cmd>' command` | Re-issue with correct argc per the Redis docs. |
| `value is not an integer or out of range` | Sent for INCR/DECR-class commands when the existing value isn't a parsable i64. Use `SET` to overwrite. |
| `no such key` | RENAME / RENAMENX / COPY / GETEX targets a key that doesn't exist. Treat as absent. |
| `kevy only supports DB 0` | `SELECT N` with N ≠ 0. kevy doesn't expose Redis's multi-DB feature; use kevy-scope or separate instances. |
| `MULTI calls can not be nested` | Already inside a MULTI block. Wait for EXEC / DISCARD first. |
| `EXEC without MULTI` / `DISCARD without MULTI` | Either pair with the matching opener or drop the command. |
| `WATCH inside MULTI is not allowed` | Issue WATCH BEFORE MULTI. |
| `pub/sub or WATCH or HELLO or RENAME not allowed inside MULTI` (v2-3a) | These commands aren't queue-safe yet (v2-3b lands the queued-RENAME orchestration). |
| `Protocol error` | The client sent malformed RESP. Reconnect + retry; if persistent, check the client library. |
| `CONFIG SET failed for '<key>': <reason>` | Field is invalid / out of range. Check `CONFIG GET` for the supported set. |
| `CONFIG REWRITE could not write <path>: <io-error>` | TOML file location is read-only or missing. Check `--config` path + filesystem permissions. |

## `-WRONGTYPE`

| Trigger | Recovery |
|---------|----------|
| `Operation against a key holding the wrong kind of value` | The key exists but is a different Redis type (e.g., HGET on a string key). Either DEL + re-create with the right type, or use a different key. |

## `-EXECABORT`

| Trigger | Recovery |
|---------|----------|
| Queued command had a syntax error during MULTI; the EXEC is aborted. | Fix the queued command and re-issue MULTI / queue / EXEC. |

## `-MOVED <slot> <host:port>` (cluster mode)

| Trigger | Recovery |
|---------|----------|
| Client sent a key whose hash-slot doesn't live on this node. | Follow the redirect: reconnect to `<host:port>` and re-issue. Cluster-aware clients (e.g., `redis-cli -c`) do this transparently. |

## `-CROSSSLOT`

| Trigger | Recovery |
|---------|----------|
| Multi-key command (MGET / MSET / DEL / EVAL with multiple KEYS) whose keys hash to DIFFERENT slots. | Either co-locate keys with `{hashtag}` syntax (so they hash to the same slot), or split the command into per-slot batches. |

## `-MISDIRECTED writer is <host:port>` (kevy-scope)

| Trigger | Recovery |
|---------|----------|
| Write landed on a node that doesn't own this key's scope. | Follow the redirect to the scope owner. kevy-cluster-rw client does this transparently. |

## `-OOM <message>` (out-of-memory)

| Trigger | Recovery |
|---------|----------|
| `command not allowed when used memory > 'maxmemory'` (v1.37+) with `noeviction` policy. | Either lift maxmemory, set an eviction policy (`allkeys-lru`), or DEL keys to free room. |

## `-READONLY`

| Trigger | Recovery |
|---------|----------|
| Write command sent to a replica node. | Send to the primary; or use `kevy-cluster-rw` which routes writes correctly. |
| `can't write against a read-only script` | Lua script was evaluated via the read-only variant (`EVAL_RO`); use `EVAL` instead. |

## `-MISCONF <message>` (v1.38+)

| Trigger | Recovery |
|---------|----------|
| `Errors writing to the AOF file: No space left on device` | Free disk space, then reissue (the in-memory state is consistent; on restart, kevy replays what made it to disk). |
| `BGSAVE failed: <io-error>` | Free disk space or fix the data_dir mount. Existing data is unaffected. |

## `-NOSCRIPT`

| Trigger | Recovery |
|---------|----------|
| `EVALSHA <sha>` where the script wasn't pre-loaded. | Use `EVAL` directly (kevy auto-caches) or pre-load via `SCRIPT LOAD`. |

## `-BUSY <message>`

| Trigger | Recovery |
|---------|----------|
| Long-running Lua script blocking the shard. | Send `SCRIPT KILL` (no-op if no script running). The kevy default `lua-time-limit` (5 s) prevents indefinite blocks. |

## `-LOADING <message>`

| Trigger | Recovery |
|---------|----------|
| kevy is still replaying AOF / loading snapshot at startup; commands rejected until ready. | Wait + retry; PING is allowed during loading. |

## Other reply classes

These don't strictly start with `-` but are wire-level errors when consumed:

- **`+PONG`** as a reply to anything other than `PING` — usually a parser desync on the client; reconnect.

## Categories that kevy DOES NOT emit (deliberately)

Per project charter:
- **`-NOAUTH`** — kevy has no AUTH (permanent design decision per `feedback-auth-tls-out`).
- **`-WRONGPASS`** — same.
- **`-NOPERM`** — no ACL system.
- **`-DENIED`** — no `protected-mode` style refusal (kevy binds 127.0.0.1 by default; deployer-side concern).

## How to update this catalog

If you add or change a code path that emits a `-<CLASS>` error, update:
1. This file — add or revise the trigger + recovery row.
2. The v1.36 (or later) chaos test `crates/kevy/tests/wire_torture_chaos.rs` if the new error is for a new class.
3. The release CHANGELOG note for the patch that introduced it.

Errors are part of the user contract; silent changes can break ecosystem libraries that pattern-match on the message.
