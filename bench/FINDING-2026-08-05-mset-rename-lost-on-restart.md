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

* ~~Cross-shard `RENAME` writes no record at all.~~ **Closed** — see
  "The third hole, closed" below.
* ~~A replica carries no secondary indexes.~~ **Not a gap — the
  documented contract**, checked rather than assumed:
  `docs/replication.md` says a replica *"declares its own
  indexes/views/aggregates over the replicated data"*, and
  `bench/repligate.sh:133` runs `IDX.CREATE` + `IDX.QUERY` **on the
  replica** as a gate case. The catalog is deliberately not propagated;
  a replica that never declared an index answering `-ERR no such index`
  is the design working. Nothing to do.

## The third hole, closed

Cross-shard `RENAME` wrote nothing at all: `RenameTake` removed the
source, `RenamePut` placed the value, both silently, so a restart
reverted the whole thing. The code said so out loud — *"cross-shard
RENAME works in-memory but is not replayed through AOF"* — deferred
because a faithful value record was assumed to need MIGRATE/RESTORE
binary frames. **It did not.** `BGREWRITEAOF` already renders any value
+ TTL as replayable commands, streams included; that serializer is now
reachable (`kevy_persist::value_as_v1_frames`) and the destination
records its put through it. One implementation of the per-type mapping —
the one that already has to be right.

The delete was the part that needed thought, and it is why this was
worth a round of its own rather than a quick patch:

* **Not at take time.** A refused `RENAMENX` rolls the value back; a
  delete already in the log would outlive the rollback as a lie.
* **Not blindly at commit time either.** A client can recreate the
  source between the take and the commit. Its `SET src …` is already in
  the log, so appending a delete *after* it would replay away a value
  that is really there. The commit checks the key is still absent.
* **The two halves are not atomic** — they live in two shards' AOFs. A
  crash between them replays the key under both names. That is the
  chosen direction: a duplicate is recoverable by hand, a vanished key
  is not.

  **And it is measured, not asserted.** Suppressing the source's record
  (the exact window a crash opens) and restarting gives `src` and `dst`
  both holding the value, DBSIZE 2 — the contract does what it says. A
  crash contract nobody has executed is a wish; this one was run.

Measured across a restart, `appendfsync always`: string (TTL carried),
hash, list, set, zset all arrive with the source gone; a refused
cross-shard `RENAMENX` leaves both keys untouched. The regression test
asserts its fixtures really straddle two shards, so a same-shard pair
cannot quietly make it pass.

## What was run against the finished branch

* `cargo test -p kevy` — 92 suites, 0 failed.
* **`crashgate`** — the SIGKILL matrix these changes sit under: 6 kill
  cells (append/rewrite/snapshot/feed × everysec/always/4-shard), 4
  windowed cells, 5 injected-damage cells. **All PASS.** Worth running
  because this work changed *what gets written*, which is exactly the
  surface that gate audits.
* **`perfgate`** — 12 angles, all above floor.

## The same lens, pointed at the sibling — and it came back clean

The AOF is one writer of the data; the **snapshot** is the other, with
its own format and its own loader. Having found three holes in one pair,
the honest move was to ask the same question of the other rather than
assume it was fine.

Measured: string + TTL, hash, list, set, zset, hash-field TTL and stream
all round-trip through `SAVE` → restart with the AOF off. **No bug.**
A negative result is still a result, and it is now a test
(`every_value_type_round_trips_through_a_snapshot`) that asserts a dump
exists and no AOF does — so it cannot pass on a path it is not testing.
Prior coverage was strings and stream groups only.

## The lesson worth keeping

Both bugs, and both index bugs found the same day, were invisible to
every test because the tests exercise **one** path each: the client
path answers correctly, so a client-driven test passes. What none of
them did was *write on one path and read on another* — write with a
client, read after a restart; write on a primary, read on a replica.

The cheap general check: for any verb the engine records itself, assert
the record round-trips through **every** reader — replay and replica —
not just that the command answered `+OK`.
