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

## Finding 2 — not crash-atomic under `Fsync::Always`

Confirmed, including your careful distinction between what you verified
and what you derived. `Aof::begin_group`/`end_group` existed, the doc
comment at `ops_atomic.rs:6` described them, and grep confirms they had
**never been called** from `kevy-embedded`. Both entry points now wrap
their commit loop in a group.

So both consequences are addressed: a block of N mutations costs one
fsync, and there is no window in which half a transaction is durable.

I did not empirically trigger the crash window either — verifying it needs
`kill -9` timed inside the commit loop, which is a test harness we do not
have. Stated plainly so you can weigh it: **finding 2's fix is
source-verified, not crash-verified.** If that matters for your risk
assessment, say so and it is worth building the harness.

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
