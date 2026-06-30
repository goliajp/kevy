# Error reply catalog

A lookup table of every wire-level error reply kevy emits, what triggers it, and what the client should do next.

## How to read this

kevy errors travel over RESP as a simple-error string: `-<PREFIX> <message>\r\n`. The first whitespace-separated token is the **prefix** (`ERR`, `WRONGTYPE`, `MOVED`, `CROSSSLOT`, ...). Client libraries typically surface this as an error variant or exception whose `.kind` / `.code` / class name matches the prefix; the rest of the line is a human-readable message.

This catalog is grouped by prefix. If you handle errors structurally (recommended), match on the prefix; the trailing message is intended for logs and operators, not for parsing.

Errors are part of kevy's user-facing contract. Adding, renaming, or repurposing a prefix is a breaking change for clients that pattern-match.

## Core reference

| Prefix | Triggers when | Recovery / next step |
|--------|---------------|----------------------|
| `-ERR unknown command '<cmd>'` | Command name is not implemented or not recognized. | Check README command coverage; verify spelling. |
| `-ERR wrong number of arguments for '<cmd>' command` | argc doesn't match the command's accepted shapes. | Re-issue with the documented argument count. |
| `-ERR value is not an integer or out of range` | INCR/DECR-class command on a value that isn't a parsable i64, or numeric arg out of range. | Use `SET` to overwrite with a parsable integer; clamp the input. |
| `-ERR no such key` | RENAME / RENAMENX / COPY / GETEX targets a key that does not exist. | Treat the key as absent (use `EXISTS` to pre-check if needed). |
| `-ERR kevy only supports DB 0` | `SELECT N` issued with N ≠ 0. | kevy has no multi-DB; use separate instances or namespaced keys. |
| `-ERR MULTI calls can not be nested` | `MULTI` sent while already inside a MULTI block. | Wait for `EXEC` / `DISCARD` before opening another transaction. |
| `-ERR EXEC without MULTI` | `EXEC` sent with no open transaction. | Pair with `MULTI`, or drop the command. |
| `-ERR DISCARD without MULTI` | `DISCARD` sent with no open transaction. | Pair with `MULTI`, or drop the command. |
| `-ERR WATCH inside MULTI is not allowed` | `WATCH` sent inside a MULTI block. | Issue `WATCH` before `MULTI`. |
| `-ERR <cmd> not allowed inside MULTI` | A command that isn't queue-safe (pub/sub, `WATCH`, `HELLO`, `RENAME`) was queued inside MULTI. | Issue these outside the transaction. |
| `-ERR Protocol error` | Inbound bytes are not valid RESP. | Reconnect and retry; if persistent, audit the client serializer. |
| `-ERR CONFIG SET failed for '<key>': <reason>` | Unknown CONFIG field or value out of range. | `CONFIG GET *` to see supported fields and current values. |
| `-ERR CONFIG REWRITE could not write <path>: <io-error>` | Config TOML path is missing or not writable. | Check `--config` path and filesystem permissions. |
| `-WRONGTYPE Operation against a key holding the wrong kind of value` | Command run against an existing key of a different Redis type. | See [Wrong-type rules](#wrong-type-rules). |
| `-EXECABORT Transaction discarded because of previous errors.` | A queued command had a syntax error during MULTI; EXEC refuses the batch. | Fix the offending queued command, then `MULTI` / queue / `EXEC` again. |
| `-MOVED <slot> <host:port>` | Key's hash slot is not owned by this node. | See [Cluster-routing replies](#cluster-routing-replies). |
| `-CROSSSLOT Keys in request don't hash to the same slot` | Multi-key command spans more than one hash slot. | See [Cluster-routing replies](#cluster-routing-replies). |
| `-MISDIRECTED writer is <host:port>` | Write landed on a node that doesn't own this key's scope. | See [Cluster-routing replies](#cluster-routing-replies). |
| `-QUIESCED migrating to <host:port>` | The slot or scope is mid-migration and frozen on this node. | See [Cluster-routing replies](#cluster-routing-replies). |
| `-OOM command not allowed when used memory > 'maxmemory'` | Write-class command with policy `noeviction` after the limit is exceeded. | Raise `maxmemory`, set an eviction policy (e.g. `allkeys-lru`), or `DEL` to free room. Existing data is intact. |
| `-READONLY You can't write against a read only replica.` | Write command sent to a replica node. | Send to the primary, or use a routing client. |
| `-READONLY can't write against a read-only script` | Script was evaluated via `EVAL_RO` / `EVALSHA_RO` and attempted a write. | Use the writable `EVAL` / `EVALSHA` variant. |
| `-MISCONF Errors writing to the AOF file: <io-error>` | AOF append failed (commonly `ENOSPC`). | Free disk space; in-memory state remains consistent; restart replays whatever reached disk. |
| `-MISCONF BGSAVE failed: <io-error>` | Background snapshot writer failed. | Free disk space or repair the `data_dir` mount. Live data is unaffected. |
| `-NOSCRIPT No matching script. Please use EVAL.` | `EVALSHA <sha>` requested a script not in the cache. | Call `EVAL` directly (kevy auto-caches) or `SCRIPT LOAD` first. |
| `-BUSY Script is running.` | A long-running Lua script is blocking the shard. | `SCRIPT KILL` to interrupt (no-op if nothing is running). The `lua-time-limit` config caps runaway scripts. |
| `-LOADING kevy is loading the dataset in memory` | Server is replaying AOF or loading snapshot at startup. | Wait and retry; `PING` is accepted during loading. |

### Prefixes kevy never emits

By design, kevy does not authenticate or authorize at the protocol layer; these Redis-compatible prefixes are deliberately absent:

- `-NOAUTH` — no `AUTH` command surface.
- `-WRONGPASS` — no password check.
- `-NOPERM` — no ACL system.

If your client expects these to be possible, treat them as unreachable on kevy. Access control is delegated to the deployment perimeter (kevy binds `127.0.0.1` by default).

## Wrong-type rules

A `WRONGTYPE` reply means the key already exists with a different Redis data type than the command expects. The rules:

- Type is decided at key creation and persists until the key is deleted (or expires).
- `DEL <key>` followed by the original command will succeed (you've reset the type).
- `EXPIRE` / `PERSIST` / `TYPE` / `OBJECT ENCODING` / `EXISTS` / `DEL` / `UNLINK` are type-agnostic and never raise `WRONGTYPE`.
- `WRONGTYPE` is never returned for a missing key; missing-key semantics follow each command's documented behavior (`GET` returns nil, `LPUSH` creates the list, and so on).

Recovery is always one of: pick a different key, or `DEL` the existing one and re-create with the intended type.

## Cluster-routing replies

These prefixes only appear when kevy is running in a routed mode (cluster or scoped). A non-cluster client may never see them.

- **`-MOVED <slot> <host:port>`** — Fires when a key's hash slot is permanently owned by another node. The client should reconnect to `<host:port>` and re-issue. Cluster-aware clients (e.g., `redis-cli -c`, `ioredis` in cluster mode) follow the redirect transparently and update their slot map.
- **`-CROSSSLOT Keys in request don't hash to the same slot`** — Fires on multi-key commands (`MGET`, `MSET`, `DEL` with multiple keys, `EVAL` with multiple `KEYS`, `SUNIONSTORE`, etc.) when the keys do not all hash to the same slot. Co-locate the keys with a shared `{hashtag}` segment, or split the command into per-slot batches client-side.
- **`-MISDIRECTED writer is <host:port>`** — Fires on a write that landed on a node that does not own the key's scope. Routing clients follow the redirect; manual clients should reconnect to `<host:port>`.
- **`-QUIESCED migrating to <host:port>`** — Fires while a slot or scope is being migrated and is frozen on this node. The client should treat it like `MOVED` and retry against `<host:port>`. Once migration completes, that node will respond authoritatively.

See the protocol notes in [docs/](https://github.com/goliajp/kevy/tree/master/docs) for the full routing model.

## FAQ

**My client treats `-MOVED` as a fatal error — how do I fix it?**
The client isn't cluster-aware. Either switch to a cluster-aware client (e.g., `redis-cli -c`, `ioredis` with `Cluster`, `redis-py` with `RedisCluster`, the kevy routing client), or wrap your driver to catch `MOVED`, reconnect to the host in the message, and re-issue.

**Single-key command came back with `-CROSSSLOT` — is that a bug?**
No. `CROSSSLOT` only fires for multi-key commands. If you see it on what looks like a single-key call, the command is actually multi-key (e.g., `EVAL` with two `KEYS`, `SUNIONSTORE` with source + destination). Use `{tag}` notation to force shared slot placement, or split the call.

**I got `-OOM` — is my data corrupt?**
No. `-OOM` is rejected at command-admission time; the write never landed. The keyspace is in exactly the state it was before the command. Free room (`DEL` / set an eviction policy / raise `maxmemory`) and retry.

**`-LOADING` keeps coming back — how long should I wait?**
For as long as the AOF / snapshot replay takes (proportional to dataset size). `PING` is accepted during loading, so health checks still work. If `-LOADING` persists indefinitely, inspect server logs — a partially corrupt AOF can stall replay.

**A queued MULTI command returned `-EXECABORT` — were any writes applied?**
No. `EXECABORT` means the transaction was rejected as a batch; nothing in the queued sequence was executed. Fix the offending command and reopen with `MULTI`.

## Updating this catalog

If you add or modify a code path that emits a `-<PREFIX> ...` reply:

1. Update the row in [Core reference](#core-reference) (or add a new one).
2. Extend the wire-level chaos test at [crates/kevy/tests/wire_torture_chaos.rs](https://github.com/goliajp/kevy/blob/master/crates/kevy/tests/wire_torture_chaos.rs) if a new prefix is introduced.
3. Note the change in [CHANGELOG.md](https://github.com/goliajp/kevy/blob/master/CHANGELOG.md) for the release that ships it.

Errors are part of the client contract. Silent message changes can break ecosystem libraries that pattern-match on them.
