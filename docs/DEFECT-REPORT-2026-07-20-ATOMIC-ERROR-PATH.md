# Defect report — `Store::atomic()` error path diverges memory from the AOF

**Reported by:** goliajp (consumer, evaluating kevy-embedded as its primary store)
**Affects:** `kevy-embedded` 3.18.0 (published) and the `feature/v4` branch — same code path in both
**Severity (consumer's view):** blocks the documented CHECK-constraint pattern; silently corrupts durability
**Not a patch:** this is a consumer report. No kevy code was modified.

## Summary

When the closure passed to `Store::atomic()` returns `Err`, writes already
performed inside that closure stay applied in memory, while their queued AOF
entries are discarded. The live process and a restarted process then answer
differently for the same key.

This lands directly on the pattern `docs/cookbook.md` §5 prescribes as the
replacement for `CHECK` constraints — "reads inside the atomic block: the app
evaluates the invariant, the engine guarantees the decision and the write
commit together". Today the decision and the commit are *not* guaranteed
together: a rejected transaction can leave its write live.

It also appears to contradict the crash-honesty line in
`docs/designing-on-kevy.md` §"The serving charter" — *"kill -9 mid-write →
replay → derived state identical to a fresh rebuild"* — since here replay
produces state that differs from the running process even without a crash.

## Reproduction

Verified empirically against the published crate (`kevy-embedded = "3.18"`),
not by code reading alone.

```rust
use kevy_embedded::{AppendFsync, Config, Store};

fn main() -> std::io::Result<()> {
    let dir = std::env::args().nth(1).expect("usage: repro <datadir>");
    let cfg = || {
        Config::default()
            .with_persist(&dir)
            .with_appendfsync(AppendFsync::Always)
    };

    {
        let s = Store::open(cfg())?;
        s.set(b"acct", b"100")?;

        let outcome = s.atomic(|ctx| {
            ctx.set(b"acct", b"999");
            Err::<(), _>(std::io::Error::other("invariant violated"))
        });
        println!("atomic returned Err            : {}", outcome.is_err());

        let live = s.get(b"acct")?.map(|v| String::from_utf8_lossy(&v).into_owned());
        println!("in-memory after rejected atomic: {live:?}");
        drop(s);
    }

    let s2 = Store::open(cfg())?;
    let replayed = s2.get(b"acct")?.map(|v| String::from_utf8_lossy(&v).into_owned());
    println!("after restart (AOF replay)     : {replayed:?}");
    Ok(())
}
```

Observed output:

```
atomic returned Err            : true
in-memory after rejected atomic: Some("999")
kevy: AOF …/aof-0.aof replayed 1 commands from 41 bytes in 0 ms (clean)
after restart (AOF replay)     : Some("100")
```

Expected (either would be defensible): `Some("100")` in both lines (the
rejected write is rolled back), or `Some("999")` in both (the write is
committed and `Err` only signals to the caller). The defect is that the two
disagree.

Note the AOF is reported `clean` — nothing surfaces the divergence.

## Mechanism

`crates/kevy-embedded/src/ops_atomic.rs:325` (3.18.0; the same shape is at
`:309` on `feature/v4`):

```rust
pub fn atomic<R>(
    &self,
    body: impl FnOnce(&mut AtomicCtx<'_>) -> io::Result<R>,
) -> io::Result<R> {
    ensure_writable(self)?;
    let mut g: RwLockWriteGuard<'_, Inner> = self.lock();
    let mut ctx = AtomicCtx { inner: &mut g, log: Vec::new() };
    let r = body(&mut ctx)?;                  // <-- early return on Err
    let log = std::mem::take(&mut ctx.log);
    for entry in log {
        let parts: Vec<&[u8]> = entry.iter().map(|v| v.as_slice()).collect();
        commit_write(&mut g, &parts)?;
    }
    Ok(r)
}
```

`AtomicCtx`'s methods mutate `inner` immediately and only *queue* the AOF
frame (`log_arg`). The `?` on line 331 returns before the commit loop, so
`ctx` is dropped with its log unconsumed: memory has the mutation, the log
does not. `atomic_all_shards` (`ops_atomic_all.rs`) has the same structure.

A second, narrower case: if `commit_write` itself fails partway through the
loop, the transaction is partially logged.

## Second finding — `atomic()` is not crash-atomic under `Fsync::Always`

Found while designing around the first issue. Reported here because it shares
the same commit loop.

`ops_atomic.rs:6` documents the behaviour as:

> AOF writes are deferred and batched into a single fsync at commit time.

That is not what happens under `Fsync::Always`. The commit loop calls
`commit_write` once per queued mutation; each one reaches
`Aof::append`, which for `Fsync::Always` does a `flush()` + `sync_data()`
**per frame** (`kevy-persist/src/aof.rs:169-172`):

```rust
Fsync::Always if self.deferred => self.dirty = true,
Fsync::Always => {
    self.file.flush()?;
    self.file.get_ref().sync_data()?;
}
```

The `deferred` arm is the group-commit path that would make the doc comment
true — `Aof::begin_group` / `end_group` exist for exactly this. But
`kevy-embedded` 3.18.0 **never calls them**: grepping the whole crate for
`begin_group|end_group|deferred` returns only the doc comment at
`ops_atomic.rs:6`.

Two consequences:

1. **Cost.** An atomic block of N mutations costs N fsyncs, not one. A cascade
   delete of 500 children is 500 fsyncs while the shard write lock is held.
2. **Durability — the significant one.** Because each frame is fsynced
   separately, a crash between frame *k* and *k+1* leaves a **durably
   half-applied transaction**. Replay then yields a state the application never
   agreed to. The atomic block gives isolation (the shard lock) but not
   durable atomicity.

### Both consequences are now empirically confirmed

Harness: open a store, run one `atomic` block of N `SET`s, `kill -9` the
process at a chosen offset, reopen, count how many of the N keys survived
replay. An all-or-nothing block can only ever yield `0` or `N`.

**Cost (consequence 1)** — 50 mutations, 10 runs each, macOS, sorted ms:

```
always   : 226 236 238 262 265 270 284 294 391 441      (median ~267ms)
everysec : 5.1 5.3 6.6 7.0 7.1 7.2 7.3 7.7 8.2 9.7      (median ~7ms)
```

~38× — consistent with 50 fsyncs versus one. 267ms / 50 ≈ 5.3ms per fsync.

**Atomicity (consequence 2), `Fsync::Always`, 50 mutations:**

```
kill@ 20ms -> 1/50     kill@130ms -> 22/50
kill@ 40ms -> 8/50     kill@160ms -> 32/50
kill@ 60ms -> 9/50     kill@190ms -> 36/50
kill@ 80ms -> 12/50    kill@220ms -> 45/50
kill@100ms -> 20/50    kill@250ms -> 50/50
```

Every intermediate value appears — a linear ramp, exactly frame-by-frame
commit. The block is not all-or-nothing.

**And the fsync policy does not fix it.** Under `EverySec` the commit loop is
too fast to hit at 1ms granularity with 50 mutations, so we widened it to 5000
(commit loop ≈ 8.5ms) and killed inside that window:

```
kill@16ms -> 2678/5000
kill@18ms ->  276/5000
```

Partial transactions occur under `EverySec` too. This is expected in
hindsight: `kill -9` leaves everything already handed to `write()` in the page
cache, so it persists regardless of fsync policy. **The partial-transaction
behaviour comes from the commit loop appending frame by frame, not from the
fsync policy.** No `AppendFsync` setting makes `Store::atomic` crash-atomic.

So the two policies differ in *cost* (38×) and in *how wide the window is*,
but not in *kind*. We are adopting `EverySec` + an explicit `fsync_aof()`
barrier per transaction purely for the cost, while treating atomic blocks as
non-atomic across a crash and adding boot-time invariant reconciliation to
detect the damage.

If group commit were wired in (`begin_group` before the loop, `end_group`
after), the frames would land as one buffered run and both this finding and
the ordering half of the first finding would be addressed together. That looks
like the smallest change that would make `atomic` mean what its name says.

## Consumer impact

We are migrating goliajp (payroll, social-insurance and billing records) onto
kevy-embedded. kevy has no `CHECK`, no `UNIQUE` enforcement at write time, no
foreign keys and no exclusion constraints — by charter — so *every* invariant
in our system becomes a read-decide-write inside `atomic()`. That makes this
error path the enforcement mechanism for all of them, including a
range-overlap constraint that currently guarantees no employee has two
overlapping secondment periods.

We are proceeding, with a self-imposed discipline: **inside any atomic
closure, perform all reads and validation first, and only then write**, so the
`Err` path never carries pending writes. That is a convention, not a
guarantee — it is invisible to the compiler and one refactor away from being
violated silently.

## What would help, in our order of preference

1. **Roll back on `Err`** — buffer mutations in `AtomicCtx` and apply them to
   `inner` only at commit, so the closure's writes and its AOF entries land
   together or not at all. This makes the cookbook §5 pattern actually hold.
2. **If rollback is out of scope by design** (we understand the Redis-semantics
   argument for `MULTI`/`EXEC`), then commit the queued log even on `Err`, so
   memory and AOF at least agree, and say so explicitly in
   `docs/cookbook.md` §5 and `docs/persistence.md` — the current text reads as
   though the decision and the commit are atomic together.
3. **Either way, document the constraint** on `Store::atomic`'s rustdoc:
   "writes performed before an `Err` return are not rolled back". Right now
   the doc comment says only that queued AOF writes are committed under one
   fsync, which does not hint at the divergence.

Happy to re-verify against any fix.
