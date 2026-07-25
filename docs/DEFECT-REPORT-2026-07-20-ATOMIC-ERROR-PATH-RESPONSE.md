# Response — `Store::atomic()` error path

**Both findings confirmed and fixed on `feature/v4`.** Reproduced here
before changing anything, including your exact program. Thank you — the
report was precise enough to act on directly, and the second finding was
wider than you had scope to see.

## Finding 1 — writes survive a rejected transaction

Confirmed. Fixed by **rollback**, which was your first preference and is
the only option under which the `cookbook.md` §5 pattern actually holds.

Your reproduction, unchanged, against the fix:

```
atomic returned Err            : true
in-memory after rejected atomic: Some("100")
kevy: AOF …/aof-0.aof replayed 1 commands from 49 bytes in 0 ms (clean)
after restart (AOF replay)     : Some("100")
```

Both lines agree, and they agree on the pre-transaction value.

**Not implemented the way you suggested**, and the reason matters for
reviewing it. Buffering mutations and applying them at commit would mean
reimplementing the semantics of all 24 `AtomicCtx` methods across strings,
hashes, sets, lists and zsets in an overlay — five families, each a fresh
chance to get one wrong, and reads inside the closure would have to see
through the overlay. Instead each key is **snapshotted on first touch**
(whole value plus remaining TTL) and restored in reverse on `Err`. The
shard write lock is held for the whole closure, so no one observes the
intermediate state; the two designs are externally equivalent. The payoff
is that all five value types travel one code path, so there is no way for
hashes to roll back correctly while zsets do not.

One deliberate choice worth flagging for your review: the keys to snapshot
are **declared by each mutating method**, not recovered from the queued
AOF argv. Recovering them would have to treat `DEL k1 k2` differently from
`SET k v`, and any mistake means "restoring" a real key that happens to
equal some value byte-string — deleting untouched data. That is the same
class of defect you reported, pointed at a different victim.

Scope note: `atomic_all_shards()` had the identical shape. It is fixed
too. A rejected transaction there was diverging several shards at once.

## CORRECTION (same day) — my first answer to finding 2 was wrong

I originally wired `begin_group`/`end_group` and wrote here that there was
"no window in which half a transaction is durable". **That claim was false
when I made it**, and your fuller report is what caught it:

> `kill -9` leaves everything already handed to `write()` in the page
> cache, so it persists regardless of fsync policy. **The partial
> transaction comes from the loop shape, not from fsync.**

That is exactly right, and it is the sentence that sent me back to
measure instead of reason. Group commit only defers the *fsync*. The AOF
writes through a 256 KiB `BufWriter`, so frames still reach the kernel
whenever that buffer fills — and after `kill -9` the kernel keeps them.
Building your harness and running it:

```
n=20000 (~760 KB of frames, 3x the buffer), group commit only:
  kill@12ms -> 6393/20000        <-- a durable half-transaction
```

So my first fix bought atomicity only for transactions that happened to
fit in the write buffer, with an undocumented cliff at 256 KiB. For a
consumer choosing this engine for payroll on the strength of that
sentence, an unqualified guarantee that silently depends on transaction
size is worse than no guarantee.

## Finding 2 — not crash-atomic under `Fsync::Always`

Confirmed, including your careful distinction between what you verified
and what you derived. `Aof::begin_group`/`end_group` existed, the doc
comment at `ops_atomic.rs:6` described them, and grep confirms they had
**never been called** from `kevy-embedded`. Both entry points now wrap
their commit loop in a group.

Fixed properly on the second attempt, with **transaction markers in the
AOF** — the WAL answer, and the only one whose correctness does not depend
on how much of the log happened to be flushed.

`begin_group` now writes a begin marker and `end_group` writes a commit
marker; replay buffers every frame after a begin and applies the batch
only on seeing the matching commit, discarding it at EOF. "Was this
transaction finished" becomes a property of the log itself. The markers
ride as ordinary v2 records holding a one-element multibulk whose name
starts with a NUL — no format change, no possible collision with a RESP
verb, and an older reader sees a command it rejects rather than a corrupt
frame. (v1 logs have no envelope and cannot express the boundary; they
gain this on their first rewrite to v2.)

Re-measured with your harness, same sizes:

```
n=20000  (3x buffer, 10 samples) : only 0/20000 or 20000/20000
n=100000 (15x buffer, 12 samples): only 0/100000 or 100000/100000
```

The transition also moved later, as it should — the commit marker is
written last, so nothing counts until it lands.

Four unit tests pin it: an uncommitted transaction applies nothing, a
committed one applies every frame, plain non-transactional appends are
unaffected, and a committed transaction survives a torn one written after
it (the case where a careless buffer reset loses both).

**Verification status, precisely:** crash-verified now, not just
source-verified — your harness shape, `kill -9` at swept offsets, at 3x
and 15x the write buffer. What is *not* covered is power loss: `kill -9`
leaves the page cache intact, so these runs exercise process death, not
media loss. Under `Fsync::Always` the commit marker is inside the synced
run, so power loss should behave the same, but I have not tested it and
will not claim it.

## On your self-imposed discipline

You wrote that you would validate before writing inside every atomic
closure, and called it "a convention, not a guarantee — one refactor away
from being violated silently".

**You should be able to drop it.** That is what the rollback is for. Five
tests now pin the contract: an overwrite rolls back, a key the closure
created is removed, all five value types restore, a key written repeatedly
lands on its pre-transaction state rather than an intermediate one, and
`Ok` still commits. If any of those regress, CI fails.

Keeping the discipline anyway costs nothing and is good practice. But it
should no longer be load-bearing for your range-overlap constraint.

## Documentation

You asked for the constraint to be documented either way. Rather than
document the old behaviour, three places now state the new guarantee **and
that it did not hold before 4.0**:

- `Store::atomic` rustdoc — rollback semantics and one-fsync commit
- `docs/cookbook.md` §5 — the CHECK-constraint pattern, with the
  rejection guarantee made explicit
- `docs/persistence.md` — group commit, noting the previous text
  described behaviour the code never had

The old text promised things the implementation did not do. Correcting it
without saying so would leave the next reader unable to tell which version
they are on.

## What this does not cover

**3.18.0 is affected and no fix is released for it.** This work is on the
`feature/v4` branch. Whether a 3.18.x patch release happens is a release
decision, not one made here — flagging it because you are on the published
crate today.

Re-verification against this fix is welcome, particularly on the
range-overlap constraint, which exercises a longer closure than the
reproduction does.
