# Two ways a write reached the store without reaching the index

Found while writing the I2 design round (`.claude/rfcs/2026-08-05-v5-i2-single-hop-index.md`),
by tracing where secondary-index maintenance is actually hooked. Both are
correctness bugs on a shipped surface (v4.1.1), both reproduce in a few
commands, and both are fixed on `fix/idx-drift-on-multikey-writes` with
regression tests that fail on the parent commit.

## The invariant they break

The master plan lists **I3 — derived-by-construction** as an *inherent*
property, not a feature: *"写路径同步维护、永不漂移、可从数据重建 ——
这正是 S3(没有 DBA)的技术实现"*
(`.claude/plans/2026-07-26-v5-arc-design-input.md:44`). The whole IDX
surface rests on it: nobody reindexes, because the write path maintains.

It held for exactly one shape of write.

## Bug 1 — a multi-key delete left the rows in the index

```
HSET row:0..row:19 age …        # 20 rows, 4 shards
IDX.CREATE byage ON PREFIX row: FIELD age TYPE i64 KIND range
DEL row:7 row:11                # multi-key
→ IDX.QUERY still returns row:7 and row:11, forever
→ IDX.COUNT counts them
→ IDX.VERIFY: drift 2
```

The reply carries the deleted row's sort value with `nil` hydration —
a row that does not exist, answered as if it did. Six seconds later it
is still there: nothing reconciles. `UNLINK` behaves the same, and a
cross-shard `RENAME` leaves the old key indexed.

**Cause.** `Commands::on_write` — the synchronous maintenance hook — was
called from exactly one place, `exec_dispatch::post_write_housekeeping`,
and only when the resolver produced a single `key_idx`. Every op that
routes by key *without* one — multi-key `DEL`/`UNLINK`, `MSET`, the
cross-shard `RENAME` and `LMOVE` two-steps, the `*STORE` destinations —
executes on the owning shard through `exec_op`, bumps the key for WATCH,
and never told the index.

**Fix.** WATCH invalidation and index maintenance are the same event —
"this key changed" — so they now travel together in one helper,
`Shard::note_key_mutated`, and every mutating op calls it. The next op
that mutates a key gets this right by using the helper rather than by
remembering to.

## Bug 2 — migrated rows never entered the index, and nothing could tell

```
MOVE-SCOPE-INGEST row: <frames for row:50, row:51>
→ +OK 2, HGET row:50 age → 60          (the rows are really there)
→ IDX.QUERY: blind to both, forever
→ IDX.VERIFY: drift 0                  (!)
```

**This direction is worse**, because no tool can see it. `IDX.VERIFY`
audits index entries against the store — it catches entries pointing at
rows that are gone (bug 1) but not rows that no index entry points at.
A node that received a scope migration silently under-answered every
indexed query against the migrated prefix, and reported itself healthy.

**Cause.** `MOVE-SCOPE-INGEST` replays the source's frames through
`dispatch::dispatch_into` directly (`ops/scope_move.rs`), which is not
the write path.

**Fix.** The ingest refreshes the derived structures per applied key.
Every emitted frame is `<VERB> <key> …` by construction
(`ops/scope_move_emit.rs`), so the key is `argv[1]`.

## The third path, now measured — and it was a bigger bug

The suspicion recorded here was that a replica takes bug 1 through the
apply path. Measured with the proper harness, it is **wrong in its
premise and worse in its consequence**:

* A replica carries **no secondary index at all** — `IDX.CREATE` is not
  propagated, so every `IDX.QUERY` there answers `-ERR no such index`.
  There is no index on a replica to drift.
* The probe built to settle that found the replica had never received
  the delete either. Pulling that thread: **`exec_op` never pushed
  anything to replication**, and `MSET` / `RENAME` did not survive a
  plain restart — the AOF record was written in a verb the replay
  dispatcher could not execute.

Full account and fixes: `FINDING-2026-08-05-mset-rename-lost-on-restart.md`.
The lesson the suspicion got right is the one that matters: a hook or a
record that is exercised on only one path is only correct on that path.

## What was checked and found healthy

* **Lua** (`EVAL` calling `DEL` / `HSET`, including creating a new row) —
  index maintained, `drift 0`. The Lua bridge does reach the hook.
* **Single-key `DEL`** — always worked; it has a `key_idx`.
* **`FLUSHALL`** — has its own hook (`on_flush`) and was never affected.

## What this says beyond the two fixes

1. **A hook with one call site is a hook with one call site.** The
   invariant was written as "the write path maintains the index", but
   "the write path" turned out to mean one of several paths that write.
   Pairing the maintenance with WATCH invalidation makes the coupling
   structural instead of remembered.
2. **`IDX.VERIFY` had a blind direction — now closed.** It could not
   detect rows missing from the index, which is bug 2's whole class.
   `TABLE.VERIFY` already computed exactly that (`missing`: "derives a
   value, has no entry — the class a drift walk cannot see"), so
   `IDX.VERIFY` now reports it *from the same classifier* rather than a
   second implementation.

   Closing it turned up a latent false-alarm in the existing one: a
   windowed path slides old rows into cold segments on purpose, and
   neither face excluded them, so a slid row counted as a hole. Both
   faces now skip rows whose window value sits below the boundary. The
   counter was harmless while only `TABLE.VERIFY` carried it and no test
   exercised a slid windowed table; it would have started crying wolf
   the moment it reached the IDX face.
3. **The I2 design round has to inherit this.** A global secondary index
   (index partitioned by value, read single-hop) multiplies the number
   of write paths that must announce themselves — it turns one local
   call into a cross-shard message. The RFC's slice I2-s2 (write side)
   should start from the *full* list of paths that mutate a key, which
   this finding is the first honest enumeration of.
