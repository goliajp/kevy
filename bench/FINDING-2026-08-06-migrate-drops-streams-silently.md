# The migration tool dropped a type and reported success

The lens that produced six defects today — **write on one path, read on
another** — had been pointed at AOF replay, replica apply, snapshot
round-trip, the change feed, index maintenance and the cold tier. Not
at the migration tool. Migration day is the one blocker on the RDS
ledger that nothing had closed, so that is where it went next.

## The defect

A store with 4007 keys, one of them a stream:

```console
$ kevy-cli export -p 6431 dump.resp
exported 4006 keys -> dump.resp
$ echo $?
0
```

Import into a fresh server: 4006 keys, and `TYPE m:stream` answers
`none`. **The stream is gone, the tool said nothing, and it exited 0.**

## Why it was silent

The exclusion is deliberate — the type match's catch-all arm returns
"no frames", with a comment saying streams are out of the rebuild set.
The problem is that **"no frames" already meant something else**: a key
that vanished between `SCAN` and the read, which is a race the walk
expects and correctly ignores.

Two different facts shared one return value, so the deliberate skip was
counted as the expected race. `export_key`'s own doc comment recorded
the wrong one of the two:

> Returns false if the key vanished between SCAN and read.

A decision recorded only in a comment, at a place where the caller
cannot see it, is not a decision the user ever gets told about.

## The fix: make the reason survive the return

`rebuild_frames` now answers with which it was — `Frames`, `Vanished`,
or `UnsupportedType` carrying the type name. `run_export` accumulates
the skips and **returns** them rather than printing (a library that
prints cannot be asserted on), and the binary reports:

```console
kevy-cli export: SKIPPED 1 key(s) of type 'stream' — nothing here
rebuilds that type, so they are NOT in this file
exported 4006 keys -> dump.resp
```

**`copy-prefix` had the same hole**, and fixing export alone left it —
same rebuild set, same dropped types, and it printed `copied N keys`.
Measured before: a 3-key prefix with one stream copied 2 and said
nothing. Both commands now report through one shared reporter.

### The test asserts the invariant, not the sentence

> Every type the engine accepts must leave a walk either **in the
> output** or **in the skipped report**.

A message can be reworded; the invariant is what would catch the next
type added without a rebuild verb. It fails on a store where a type is
in neither.

## What this does not mean — the verification step is not blind

Worth measuring rather than assuming, because a verifier that shares
the tool's blind spot would turn a silent loss into a *confirmed* one.
`docs/migration.md` tells you to compare `PREFIX.DIGEST` after a
migration. Does the digest see streams?

| | keys | digest |
|---|---|---|
| source (with the stream) | 3 | `4d885664f880f333` |
| destination (after the export that dropped it) | 2 | `803b2062fcd19c06` |

**It does.** Anyone who followed the documented procedure would have
caught this — the counts and the digests both disagree. So the failure
was "the tool did not tell you, and only the verification step could",
not "the loss was undetectable". That is the correct severity, and it
is why the fix is a report rather than a refusal: the tool's job here is
to be honest about its own coverage, not to block a migration whose
operator may well not have streams.

## The doc said something stronger than the tool does

> The leading `DEL` per key makes replay **rebuild from scratch** —
> genuinely idempotent for **every type**.

Idempotent for every type it emits — five of six. All three language
versions now name the one it leaves behind and show the line to look
for.

## The engine already knows how to move a stream — just not from there

`MOVE-SCOPE` is the *other* rebuild-frame emitter: server-side, used to
hand a prefix to another node during a reshard. Measured (a test now
pins it):

| | types carried | TTL |
|---|---|---|
| `MOVE-SCOPE` (server, reshard) | **six — string, hash, list, set, zset, stream** | ✅ `PEXPIREAT` |
| `kevy-cli export` / `copy-prefix` (client, migration) | five — **no stream** | ✅ `PEXPIREAT` |

So the CLI's gap is **not fundamental**. The engine rebuilds a stream
perfectly well; the client-side path, which reconstructs over RESP with
`TYPE` + `GET`/`HGETALL`/`LRANGE`/…, simply never grew the `XRANGE` →
`XADD`-with-explicit-ids arm the server-side emitter has.

Whether to close that is a product call — the docs now tell you to move
streams separately, which is honest and may be enough. What the
asymmetry does establish is that "streams cannot be rebuilt from
frames" would be the wrong reason to leave it.

**Both emitters share the shape that caused this.** `MOVE-SCOPE`'s
catch-all is a bare `continue`, so a type added to the engine without
an arm there reshards into nothing, just as silently. It now has a test
pinning all six, which is the cheap half of the same lesson.

## The pattern, seventh instance

Every defect this lens has found today has the same shape: **two facts
sharing one channel.** A verb that meant both "applied" and "recorded",
an exemption that meant both "slid out" and "lost", a tombstone that
meant "shadow this row" without saying which segment, and now a return
value that meant both "raced" and "unsupported".

The fix each time was not new logic. It was **giving the second fact
its own way to be seen**.
