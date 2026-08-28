# FINDING 2026-08-29 — fourteen verbs the engine had and the wire did not

**Status**: thirteen wired, one named with what it needs. The
ledger that recorded them was accurate; nothing here is a correction of
it. What is new is that the ledger had no expiry, and no one had asked
what it would cost to close.

## What was found, and how

Following the differential register (`differential_wire_vs_embedded.rs`)
down one layer. `ops_table::KNOWN_GAPS` carries a family it calls F3 —
"exists in kevy-store + embedded but not on the server wire" — and the
differential auto-excuses every corpus line whose verb is in it, which
is correct behaviour and also very quiet behaviour. Reading the list
out loud is what made it visible:

```
SETBIT GETBIT BITCOUNT BITPOS BITOP GETRANGE SETRANGE
LINSERT COPY TOUCH TIME GETEX ZREVRANGE HINCRBYFLOAT
```

Fourteen ordinary Redis commands. Every one of them is implemented in
`kevy-store`, and every one is answered by the embedded facade — which
is how the differential could compare them at all: it was comparing a
real answer against `-ERR unknown command`.

`.claude/scope-decisions.md` does not put them out of scope. The "F3"
that appears there is a different F3 (quorum). So this was not a
decision anyone had made; it was a gap that had been registered and
then left, which is a different thing and reads the same from outside.

## What each one actually needed

Nothing in the engine. The work is registration, in seven places:

| where | what it holds |
|---|---|
| a `dispatch_*` table | the verb → `Store` call |
| `cmd_class.rs` | write / growing-write / keyspace-notification class |
| `cmd_resolve.rs` | routing, when the default is wrong |
| `ops_table.rs` | surfaces, notify class — and the gap ledger row to retire |
| `verb_arity.rs` | the arity column both surfaces read |
| `verb_meta/*` | the documented face: syntax, complexity, compat |
| replication | the replay path, for a write |

Eleven of the fourteen needed no routing at all. `route_for_verb`'s
default arm already sends any verb with two or more arguments to
`Route::Single(1)`, which is right for every single-key verb, and a
one-argument verb to `Route::Local`, which is right for TIME.

The three multi-key verbs were where the estimate went wrong, and it is
worth writing down because the error has a shape. The first pass of
this file said all three "need a route of its own, and that is a
decision about cross-shard behaviour rather than a table row". For
**TOUCH** that was simply false, and reading one line would have shown
it: the embedded facade's `touch` is `self.exists(keys)`, because this
engine has no idle-time clock to reset — the existence probe already
does the eviction bookkeeping. `Route::ExistsKeys` emits `Op::Exists`
and sums, which IS touch's contract here. It went in as an alias, one
match arm shared with EXISTS so that two arms cannot answer differently.

Grouping three things by a surface property — "they name more than one
key" — and then assigning the group one cost is how an estimate gets
made without looking at any of the three.

**COPY** was the one that needed the orchestration, and it is written:
`kevy-rt/src/exec_copy.rs`. It is half the length of `exec_rename.rs`,
and the reason is one word in the first step. RENAME *takes* the source,
so an NX-refused put has to give it back — hence a third step, a
`RenameStep::Restore`, and a documented data-loss race. COPY *clones*: a
refused put leaves both keys exactly as they were, and the worst a crash
between the two steps can do is not write the destination. There is no
rollback to arrange because nothing was removed.

Same-shard pairs take one atomic op, as Redis's COPY does — but through
the same orchestrator slot at its final step, which is the correction to
the first draft: giving that path an `Agg::First` meant the fold had no
arm for `Part::CopyPutDone`, so it was dropped and the client got the
materialize fallback, `-ERR internal error`. The differential caught it
on the first run.

**BITOP** remains. It gathers N source strings across shards, combines
them byte-wise under AND/OR/XOR/NOT, and materialises at a destination
that sits at `args[2]` rather than `args[1]`. `Route::ZAlgebraStore` is
the right shape with the wrong payload — it combines set and zset
members, not raw bytes.

The ledger is one row now, not fourteen.

## What the COPY tests had to be

`crates/kevy/tests/copy_cross_shard.rs`, eight shards, and the rule
`list_move_cross_shard.rs` wrote after RPOPLPUSH lost 11 of 12 elements:
**assert what the destination holds, never just the reply.** A test that
read the reply would pass against a COPY routed by `args[1]`, which
writes the copy into the source's shard where no later read of the
destination looks.

Two of the tests are about the instrument rather than the engine:

* One asks `kevy_rt::shard_of_key` — the function the server routes with
  — how many of the twelve pairs actually split. Ten do. "Eight shards,
  so surely they do" is an assumption; this is a measurement, and
  without it the file could be testing the same-shard path under a
  cross-shard name.
* The durability test was verified red before it was trusted green.
  `op_copy_put` logs through the same `log_value_placed` that RENAME's
  cross-shard put uses — but "the same call" is a claim about the code,
  not about the file. Removing that one line makes the test report
  "10 of 10 cross-shard copies were remembered but never written down",
  by name.

## What the registries said while this was done

Three things, and all three were the instruments working:

**`server_surface_has_dispatch_literals` went blind, and said so.**
Splitting `dispatch_string` out of `dispatch.rs` (which had grown to
within thirty lines of the 500-LOC rule) moved APPEND's arm into a file
that was not in the check's hand-written list of eleven `include_str!`
lines — so the check reported that nothing on the server implemented
APPEND. A source list maintained by hand answers a question about the
files someone remembered. It now walks the tree, with a floor, because
an empty read would fail the forward assertion loudly and pass the
inverse guard — the one that says a gap is still open — in silence.

**Two OP_TABLE notify columns were wrong, and had never been asked.**
`GETEX` was recorded as notifying `String` and `SETRANGE` as notifying
nothing; the parity test against `notify_class_for_verb` caught both the
moment the verbs reached the server. Neither column had ever been
exercised, because the notify class is a server concept and these were
ESTORE-only. SETRANGE now says String. GETEX now says None, and the row
says why: Redis fires `expire` for the EX/PX form and nothing for the
bare one, never `getex`, and this engine keys the event NAME off the
verb — so any class here would publish a name Redis does not have.

**The short-call probe's premise was false for nine verbs.**
`both_surfaces_refuse_a_short_call_the_same_way` sends each shared verb
with no arguments and requires the two surfaces to refuse alike, on the
stated premise that "every verb here takes at least one operand". TIME
does not, and neither do DBSIZE, FLUSHALL, RANDOMKEY, IDX.LIST,
IDX.ADVISE, TABLE.LIST, VIEW.LIST or FEED.SHARDS. For eight of the nine
the two surfaces happened to answer identically, so the probe passed
while asserting something it had not tested. The premise is now read
from the arity column both surfaces share — `arity_ok(name, 1)` — so
the skip list is derived rather than remembered, and it prints.

## Measured

`differential_wire_vs_embedded.rs`, before and after:

```
before   157 of 174 agree byte-for-byte;  17 diverge (17 named)
after    169 of 174 agree byte-for-byte;   5 diverge  (5 named)
shared surface 112 → 125 verbs; register 121 driven + 4 named
```

The five that remain: three IDX.* the facade does not carry
(EXPLAIN / VERIFY / REBUILD), TABLE.VERIFY's tick timing, and BITOP.

The whole test suite for the three crates this touches: 1,119 passing,
none failing, across 122 targets.

## What was not done

BITOP, described above by what it needs rather than by the group it was
first sorted into.
