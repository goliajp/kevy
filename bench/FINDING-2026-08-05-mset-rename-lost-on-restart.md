# MSET and RENAME were acknowledged, durable-looking, and gone

Found while chasing a *different* suspicion — that a replica misses the
index maintenance for a multi-key `DEL`. The replica turned out not to
carry indexes at all, and the probe that was supposed to settle that
question surfaced something larger: **the replica had not received the
delete either.** Pulling that thread found two data-loss bugs on a
shipped surface (v4.1.1), one of them across a plain restart.

Fixed on `fix/idx-drift-on-multikey-writes`, with regression tests that
fail on the parent commit. A third hole is measured and left open.

## What was measured

`appendfsync always`, write, kill, start again, read:

| verb | survives restart? |
|---|---|
| `SET k v` | yes |
| `DEL a b` (multi-key) | yes |
| `LMOVE L1 L2 LEFT RIGHT` | yes |
| **`MSET m:1 a m:2 b`** | **no — every key gone** |
| **`RENAME src dst` (same shard)** | **no — reverted: `src` alive, `dst` missing** |

And against a live replica (`REPLICAOF`, polled to settle, not slept on):

| verb | reaches the replica? |
|---|---|
| `DEL row:2` (single key) | yes |
| **`DEL row:3 row:4`** | **no** |
| **`MSET`** | **no** |
| **`RENAME`** | **no** |

`MSET` answered `+OK`. The value read back correctly. `INFO` was clean.
The data was gone at the next start.

## Two holes, one shape

**The path that writes is not the path that replays.**

1. **`exec_op` never pushed to replication.** Every mutation the runtime
   routes as an `Op` — multi-key `DEL`/`UNLINK`, `MSET`, the cross-shard
   `RENAME` and `LMOVE` two-steps, the `*STORE` destinations, `FLUSHALL`
   — appends its effect frame to the AOF (`self.log`) and stopped there.
   The replication push (`push_mutation`) existed in exactly three
   places, all on the single-key dispatch path. So those verbs were
   durable and unreplicated.

   Sharper still: the list-move effect records were built only
   `if self.aof.is_some()`, so a **replication-only deployment produced
   no record at all** — the move reached neither disk nor replica.

2. **The AOF record was written in a verb nothing could replay.**
   `MSET` and `RENAME` are routed-only: the client path resolves them to
   `Route::MSet` / `Route::Rename` and executes an `Op`. But replay
   (`shard_run::replay_dispatch`) and replica-apply
   (`replication_apply::apply_replica_frame`) both go through the
   **local** dispatcher, where `MSET` answered an arity error
   (`dispatch.rs`, "they only reach `dispatch` when malformed") and
   `RENAME` had no arm at all. The record went to disk in a language the
   reader did not speak.

   That comment was true when it was written and false ever since the
   engine started logging those verbs itself. It is the exact shape of
   the index-maintenance bug found the same day
   (`FINDING-2026-08-05-index-drift-on-non-dispatch-writes.md`): a
   contract that holds for one path and is assumed for all of them.

## The fixes

* `Shard::log_effect` — appends to the AOF **and** pushes to
  replication, in one call, next to each other, so an op cannot do one
  without the other. Suppressed while applying an upstream frame (a
  replica re-emitting what it just applied is how a chain loops).
* The list-move loggers build their record whenever there is anywhere
  for it to go (AOF **or** replication), not only for the AOF.
* **`FLUSHALL` is the family's one exception, and the suite said so.**
  The first cut pushed it like the rest; `feed_cdc`'s
  `flushall_bumps_generation_and_old_cursor_resyncs` failed because a
  flush already has a channel — it bumps the feed generation so every
  stale cursor gets `-FEEDRESYNC <gen> 0`, and an extra record lands at
  the offset that bump just reset to zero. It appends to the AOF only.
  Worth keeping as a shape: when a fix unifies a family, check for the
  member that already had its own way.
* `MSET` / `RENAME` / `RENAMENX` execute in the local dispatcher
  (`dispatch_replay`), which makes existing AOFs replayable — the fix
  recovers data already on disk, not just future writes. A malformed
  call still answers the arity error, which is all the client path
  relied on.

## Still open — measured, not fixed

* **Cross-shard `RENAME` writes no record at all.** `Op::RenameTake` and
  `Op::RenamePut` have no logging, so with `src` and `dst` on different
  shards the rename still reverts on restart. Fixing it needs the value
  reconstructed by type on the destination shard (the `scope_move_emit`
  idiom: `SET`/`HSET`/`RPUSH`/… per value kind) plus a `DEL` on the
  source's. That is a different piece of work and is not started.
* **A replica carries no secondary indexes.** `IDX.CREATE` is a catalog
  mutation that is not propagated, so a replica answers
  `-ERR no such index` for every `IDX.QUERY`. Whether that is intended
  (read replicas serve KV only) or a gap is a design question for the
  owner, not a bug to fix quietly.

## The lesson worth keeping

Both bugs, and both index bugs found the same day, were invisible to
every test because the tests exercise **one** path each: the client
path answers correctly, so a client-driven test passes. What none of
them did was *write on one path and read on another* — write with a
client, read after a restart; write on a primary, read on a replica.

The cheap general check: for any verb the engine records itself, assert
the record round-trips through **every** reader — replay and replica —
not just that the command answered `+OK`.
