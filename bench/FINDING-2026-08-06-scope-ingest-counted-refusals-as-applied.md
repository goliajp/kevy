# A reshard counted refused rows as moved

Eighth defect from the same lens, and the one with the worst
consequence: a row that fails to land on the target becomes unreachable
from **both** nodes.

## The defect

`MOVE-SCOPE-INGEST` replays the shipped rebuild frames into the target
store. It dispatched each frame into a scratch buffer and **never
looked at the reply**:

```rust
scratch.clear();
crate::dispatch::dispatch_into(ctx, store, &argv, &mut scratch);
…
applied += 1;          // whatever the reply said
```

Measured directly, before any change. Target already holds `app:x` as a
string; the shipped frame rebuilds it as a list, so `RPUSH` is refused:

```
PROBE ingest reply  = "+OK 1"
PROBE key type after = "string"
```

**`+OK 1` for a frame that did not apply.** The list is not there and
the count says it is.

## Why that is worse than it looks

The source reads `+OK <count>` and treats the ship as successful:

```rust
Ok(count) => { ctx.state.scope.migration_commit(&prefix_owned); … }
Err(e)    => { ctx.state.scope.migration_abort(&prefix_owned); … }
```

`migration_commit` moves the prefix from *migrating* to *migrated* —
**ownership changes**. It does not delete the source data, so this is
not "gone from disk". It is worse in a subtler way:

* the **source** still holds the row but no longer owns the prefix, so
  it answers `-MISDIRECTED` and points at the target;
* the **target** does not have the row, or has a different value under
  that key;
* the operator saw a success line with a count that included the
  failure.

A row that exists on disk and cannot be reached from either node is
harder to notice than a row that is plainly missing.

## The fix: read the reply, refuse by name

`apply_ingest_frames` now inspects each dispatch reply. A `-` reply
stops the ingest and returns which key and why, so the target answers:

```
-ERR MOVE-SCOPE-INGEST: key 'app:x' refused by this node: WRONGTYPE …
```

The source then takes its `Err` branch and **aborts** the migration:
ownership does not move, the data stays where it is and stays
reachable, and the operator gets the key name and the reason instead of
a count.

Aborting is the right direction rather than partial-success reporting.
A prefix that did not arrive intact must not be declared as living
somewhere else; the safe failure for a move is "nothing moved".

Two tests pin it: a refused frame must not answer `+OK` and must name
both the key and the refusal, and an ordinary ingest still reports how
many applied.

The receiving half now lives in its own module (`scope_move_ingest`),
split by direction rather than by size: **a shipper that cannot connect
leaves the data where it is; a receiver that cannot apply must say so
loudly enough that the shipper does not commit ownership.** Those are
different failure duties and they read better apart.

## Eighth instance, same shape

Every defect this lens has found today is **two facts sharing one
channel**:

| the channel | fact it carried | fact it swallowed |
|---|---|---|
| a logged verb | "applied" | "recorded for replicas" |
| a `None` return | "key vanished mid-walk" | "type has no rebuild verb" |
| a tombstone | "shadow this row" | "…in which segment" |
| an exemption | "slid out of the window" | "lost between the structures" |
| **a dispatch reply** | **written to a buffer** | **"this frame was refused"** |

The fix is never new logic. It is giving the second fact a way to be
seen — and in every case the code that swallowed it had a comment
explaining the first fact, which is exactly why nobody looked twice.
