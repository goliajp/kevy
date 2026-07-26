# Consumer report — kevy-embedded 3.18 as a primary store

> **RESOLVED in 4.0.0 (verified 2026-07-26).** Every defect and the two
> highest-priority requests below were fixed in the published 4.0.0 crate, and
> we re-ran our harness against it to confirm:
>
> - **D1 (rejected atomic left writes live)** → fixed. `atomic()` now rolls
>   back on `Err` (`ops_atomic.rs` `rollback(&mut g, undo)`). Measured: after a
>   rejected closure the key reads `100` in memory **and** `100` after restart
>   (3.18 gave `999` in memory, `100` after replay).
> - **D2 (not crash-atomic under any fsync policy)** → fixed. Commit is one
>   bracketed AOF group (`commit_group` / `begin_group`). Measured: `kill -9`
>   inside a 50-mutation block under `Fsync::Always` yields only `0` or `50`,
>   never an intermediate count (3.18 produced every value `1..50`).
> - **D3 (doc comment described behaviour the crate lacked)** → moot with D2 fixed.
> - **R2 / F1 (no collection reads inside `atomic`)** → fixed. `AtomicCtx`
>   gained `SMEMBERS SISMEMBER LRANGE LLEN SCARD ZRANGEBYSCORE`; verified
>   `smembers`/`sismember` work inside a transaction.
> - **R1 (make `atomic` all-or-nothing)** → this is exactly D1+D2 above.
>
> The remaining items (R3 index-reads-in-atomic, R4 boot reconciliation hook,
> R6 the one-row-many-derived-keys recipe) are enhancements, not defects, and
> are not blocking us. Thank you — the turnaround was fast and the fixes match
> what the report asked for. goliajp is now on 4.0.0. The original report is
> kept verbatim below as the record.

**From:** goliajp (GOLIA K.K. internal business system — payroll, social
insurance, secondment billing, IM, wiki, git hosting)
**Against:** `kevy-embedded = "3.18"` from crates.io, and `feature/v4` where noted
**Date:** 2026-07-20 (resolution note appended 2026-07-26)
**Nature:** consumer findings. **No kevy code was modified.** Everything below
is either measured on this machine or cited to a line in the published crate.

We are replacing PostgreSQL 18 + Valkey 9.1 with `kevy-embedded` as the single
store for goliajp. This document is what we found doing it: three defects, a
set of friction points that cost us design time, and the things we would most
like to see next. It is written to be useful rather than polite — we are
committing to this engine, so its sharp edges are our problem too.

Context on scale, because it shapes which of these matter: 58 tables, 15 MB
total, largest table 1,780 rows, single process, ~5 concurrent users. Nothing
here is a throughput complaint. Every finding is about correctness or about
what the API makes hard to express.

---

## Part 1 — Defects

### D1. `Store::atomic` leaves rejected writes applied in memory while dropping their AOF entries

**Severity (our view): high.** It silently breaks the pattern
`docs/cookbook.md` §5 prescribes as the replacement for `CHECK` constraints.

`crates/kevy-embedded/src/ops_atomic.rs:325` (3.18.0; same shape at `:309` on
`feature/v4`):

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

`AtomicCtx`'s methods mutate `inner` immediately and only *queue* the AOF frame
(`log_arg`). The `?` returns before the commit loop, so `ctx` is dropped with
its log unconsumed: memory has the mutation, the durable log does not.

**Reproduced** against the published crate:

```rust
use kevy_embedded::{AppendFsync, Config, Store};

fn main() -> std::io::Result<()> {
    let dir = std::env::args().nth(1).expect("usage: repro <datadir>");
    let cfg = || Config::default().with_persist(&dir)
                                  .with_appendfsync(AppendFsync::Always);
    {
        let s = Store::open(cfg())?;
        s.set(b"acct", b"100")?;
        let outcome = s.atomic(|ctx| {
            ctx.set(b"acct", b"999");
            Err::<(), _>(std::io::Error::other("invariant violated"))
        });
        println!("atomic returned Err            : {}", outcome.is_err());
        println!("in-memory after rejected atomic: {:?}",
                 s.get(b"acct")?.map(|v| String::from_utf8_lossy(&v).into_owned()));
    }
    let s2 = Store::open(cfg())?;
    println!("after restart (AOF replay)     : {:?}",
             s2.get(b"acct")?.map(|v| String::from_utf8_lossy(&v).into_owned()));
    Ok(())
}
```

```
atomic returned Err            : true
in-memory after rejected atomic: Some("999")
kevy: AOF …/aof-0.aof replayed 1 commands from 41 bytes in 0 ms (clean)
after restart (AOF replay)     : Some("100")
```

Two distinct harms. The rejected write is **still live** — so the "CHECK
constraint" rejected nothing. And the running process and a restarted process
**answer differently** for the same key, with the AOF reporting itself `clean`.

Either outcome would be defensible on its own (roll back, or commit and let
`Err` be advisory). The defect is that the two halves disagree.

### D2. `atomic` is not crash-atomic under any fsync policy

**Severity: high**, and worse than D1 because no application-side discipline
avoids it.

The commit loop calls `commit_write` once per queued mutation. Each reaches
`Aof::append`, which for `Fsync::Always` does `flush()` + `sync_data()` **per
frame** (`kevy-persist/src/aof.rs:169-172`):

```rust
Fsync::Always if self.deferred => self.dirty = true,
Fsync::Always => { self.file.flush()?; self.file.get_ref().sync_data()?; }
```

The `deferred` arm is group commit — `Aof::begin_group` / `end_group` exist for
exactly this. **`kevy-embedded` never calls them.** Grepping the whole crate
for `begin_group|end_group|deferred` returns one hit: the doc comment at
`ops_atomic.rs:6` which claims "AOF writes are deferred and batched into a
single fsync at commit time." That comment does not describe this build.

**Measured.** Harness: run one `atomic` block of N `SET`s, `kill -9` at a
chosen offset, reopen, count survivors. An all-or-nothing block can only ever
yield `0` or `N`.

Cost, 50 mutations, 10 runs each, sorted ms:

```
always   : 226 236 238 262 265 270 284 294 391 441      (median ~267ms)
everysec : 5.1 5.3 6.6 7.0 7.1 7.2 7.3 7.7 8.2 9.7      (median ~7ms)
```

38×, consistent with 50 fsyncs versus one. ~5.3 ms per fsync.

Atomicity, `Fsync::Always`, 50 mutations:

```
kill@ 20ms ->  1/50    kill@130ms -> 22/50
kill@ 40ms ->  8/50    kill@160ms -> 32/50
kill@ 60ms ->  9/50    kill@190ms -> 36/50
kill@ 80ms -> 12/50    kill@220ms -> 45/50
kill@100ms -> 20/50    kill@250ms -> 50/50
```

A linear ramp through every intermediate value — frame-by-frame commit, plainly.

**And the fsync policy does not fix it.** Under `EverySec` the loop is too
short to hit at 1 ms granularity with 50 mutations, so we widened it to 5000
(loop ≈ 8.5 ms):

```
kill@16ms -> 2678/5000
kill@18ms ->  276/5000
```

Expected in hindsight: `kill -9` leaves everything already handed to `write()`
in the page cache, so it persists regardless of fsync policy. **The partial
transaction comes from the loop shape, not from fsync.** No `AppendFsync`
setting makes `atomic` crash-atomic; `Always` only widens the window and costs
38× more.

This matters more for us than it would for a cache consumer. kevy provides no
`CHECK`, no write-time `UNIQUE`, no foreign keys and no exclusion constraints —
by charter, which we accept — so *every* invariant in our system becomes a
read-decide-write inside `atomic`. That includes a range-overlap constraint
that currently guarantees no employee holds two overlapping secondment periods.
A crash mid-block can leave any of them broken, and nothing surfaces it.

Wiring `begin_group` before the loop and `end_group` after looks like the
smallest change that would make `atomic` mean what its name says, and it would
address the ordering half of D1 at the same time.

### D3. The `atomic` doc comment describes behaviour the crate does not have

`ops_atomic.rs:6` and the rustdoc on `Store::atomic` both say the queued AOF
writes are committed "under one fsync". Under `Always` they are committed under
N. Under `EverySec` there is no fsync at all until the reaper or an explicit
`fsync_aof`. Neither reading matches the sentence.

Small, but it is the sentence a consumer reads when deciding whether `atomic`
is safe for money, so we would rank it above its size.

---

## Part 2 — What worked well

Stated because a report of only defects would misrepresent the engine.

- **AOF torn-tail recovery is exemplary.** We truncated 17 bytes off the tail
  and replayed:

  ```
  replayed 99 commands from 2882 bytes; trailing 12 bytes were a partial frame
  (crash mid-append, recoverable)
  ```

  Correct recovery to the last complete frame, and it *says so* rather than
  reporting `clean`. The quarantine path for mid-file corruption
  (`kevy-persist/src/replay.rs`) is the same quality. This is the part of the
  system we trust most.

- **Index build and boot rebuild are a non-issue at our scale.** 2,000 rows:
  `idx_create` 2 ms, reopen-and-index-ready 1 ms. 20,000 rows: 16 ms and 11 ms.
  The documented caveat about derived state rebuilding on restart is real but
  irrelevant below six figures of rows.

- **`with_resp_listener` resolved a dilemma we thought was a real tradeoff.**
  We assumed embedded meant giving up `redis-cli` inspection and the export
  tooling. A read-only port alongside the in-process store gives both. This
  should be advertised harder — it is the feature that made embedded viable for
  us rather than a compromise.

- **The documentation is unusually honest.** `docs/designing-on-kevy.md`'s
  "Considered and refused" table and `docs/rds-workloads.md`'s "What kevy will
  NOT do" saved us days. Most projects hide this; publishing it is why our
  design converged as fast as it did.

- **`KIND text` is an upgrade, not a compromise.** Our PostgreSQL full-text
  search used the `'simple'` configuration, which does no CJK segmentation at
  all. BM25 with dictionary-free CJK bigrams is strictly better for Japanese
  and Chinese content.

---

## Part 3 — Friction that cost us design time

Not defects. Places where the API's shape forced a decision we would not
otherwise have made, listed because they are cheap to document even if none of
them changes.

### F1. `AtomicCtx` exposes 22 verbs, and the omissions drive the whole data model

`ops_atomic.rs:346`:

```
SET GET INCR INCRBY HSET HGET HINCRBY ZADD ZINCRBY ZSCORE DEL EXISTS
HDEL HGETALL HMGET HEXISTS SADD SREM LPUSH RPUSH ZREM ZCARD
```

No `SMEMBERS`, no `SISMEMBER`, no `LRANGE`, no `ZRANGEBYSCORE`, no `EXPIRE`,
and no index query.

The consequence is larger than the list suggests. **A set can be written inside
a transaction but never read back inside one.** So any child collection a
cascade delete must enumerate has to be a hash, not a set — even though a set
is the natural shape and what `docs/cookbook.md` §2 shows. We reshaped our
entire keyspace around this before writing a single store, and the decision is
expensive to revisit afterward.

Suggestion, in order of preference:

1. Add the read-only collection verbs to `AtomicCtx` (`SMEMBERS`,
   `SISMEMBER`, `LRANGE`, `ZRANGEBYSCORE`). They hold the shard lock already;
   there seems to be no consistency reason to withhold reads.
2. Failing that, say this explicitly in `cookbook.md` §2 and §10 — "collections
   you must enumerate inside a transaction should be hashes" is a one-line note
   that would have saved us a keyspace redesign.

### F2. No index read inside a transaction, which makes `KIND unique` unusable for uniqueness

`indexes.md:55` already says uniqueness is "a fence, not a lock" and suggests
`MULTI`/`WATCH`. For the embedded API there is a stronger statement available:
**an index cannot be consulted inside `atomic` at all**, so `KIND unique`
cannot participate in the check even optimistically.

We ended up implementing all 22 of our uniqueness constraints as claim keys
(`u:<constraint>:<value>` → owner id) read with `get` inside the transaction,
and using `KIND unique` for nothing. That is a fine outcome, but we arrived at
it by discovering the omission rather than by reading it.

Suggestion: a line in `indexes.md` under "Uniqueness is a fence" — "in the
embedded API, indexes cannot be read inside `atomic`; use a claim key" — plus a
cookbook recipe for the claim-key pattern. It is the single most reusable
pattern we built and it is not in the cookbook.

### F3. `MAX_INDEXES = 64` is global, and the budget model is not obvious

`kevy-index/src/catalog.rs:143`. Read naively, 58 tables against 64 indexes
looks impossible, and our first instinct was that the migration was blocked.

What actually resolves it is the modelling rule that link keys and zsets carry
relational access paths for free, so indexes are spent only on global value
ranges, text and aggregates — we need roughly 19. But that rule is implicit,
assembled from `cookbook.md` §2 and `rds-workloads.md`.

Suggestion: state the budget rule directly in `indexes.md`. Something like:
"Indexes are a scarce global budget. Parent-child access paths belong in link
keys, which cost nothing. Spend index slots only on global ranges, text and
aggregates." One paragraph would prevent a whole class of "we hit 64 halfway
through the migration" story.

### F4. Composite group keys for `KIND agg`

`GROUPBY` takes one field. Our real reporting query is
`SUM(amount) GROUP BY month` split by direction — expressible only by
materialising a composite `ym_dir` field (`"2026-07:in"`) at write time and
splitting it in the app.

That works and we are content with it. But it is a modelling idiom nobody would
invent on first contact, and it is not in the cookbook. `KIND agg` looks like it
answers `GROUP BY` and then does not answer the shape of `GROUP BY` people
actually write.

Suggestion: a cookbook recipe for composite group keys, and a note under
`KIND agg` that conditional aggregates (`SUM(CASE WHEN …)`) are expressed by
moving the condition into the group key.

### F5. `Fsync::Always` is a trap for consumers who reason from the name

We selected `Always` on first read, reasoning that financial data should not
accept acknowledged-write loss. It is 38× slower **and no safer for block
atomicity** (D2). A consumer optimising for durability picks the setting that
costs the most and buys the least.

Suggestion: `persistence.md` should say plainly that `AppendFsync` governs the
power-loss window for individual commands and does **not** make `atomic` blocks
all-or-nothing, and that for transaction-shaped work `EverySec` plus an
explicit `fsync_aof()` barrier is both cheaper and no weaker. (If D2 gets
fixed via group commit, this note becomes unnecessary and `Always` becomes the
right default again — which is the better outcome.)

---

## Part 4 — What we would most like to see

Ordered by what would change our design, not by presumed difficulty.

### R1. Make `atomic` all-or-nothing (fixes D1 and D2 together)

Buffer mutations in `AtomicCtx` and apply them to `inner` only at commit, and
bracket the commit loop in `begin_group`/`end_group`. Then a closure returning
`Err` changes nothing, and a crash mid-commit leaves the AOF either without the
block or with all of it.

This is the single change that would most improve kevy as a primary store. With
no `CHECK`, no FK and no exclusion constraints — all reasonable charter
decisions — `atomic` is the *only* place a consumer can enforce an invariant.
It needs to be the strongest primitive in the engine, and right now it is
weaker than its name promises.

We worked around D1 structurally: our closure cannot write. It reads, decides,
and returns a plan value that the framework applies afterward, so the `Err`
path provably has nothing staged. We would rather delete that machinery.

### R2. Read-only collection verbs inside `atomic`

Per F1. `SMEMBERS`, `SISMEMBER`, `LRANGE`, `ZRANGEBYSCORE`. This would let
child collections be sets — the shape the cookbook already teaches — instead of
hashes chosen to work around an API gap.

### R3. Index reads inside `atomic`, even restricted ones

Per F2. Even `idx_query … EQ` limited to a single shard would let uniqueness
and existence checks use declared indexes rather than parallel claim keys.
Claim keys are a second source of truth we now have to reconcile at boot; an
index is derived-by-construction and cannot drift. Trading a maintained
invariant for a derived one is the direction we would like to go.

### R4. A boot-time invariant reconciliation hook

Because atomic blocks are not crash-atomic (D2), we had to build boot-time
verification that rebuilds every derived key from the rows and diffs. It is the
only detector we have for damage from a crash.

This is generic. Every consumer that maintains link keys or claim keys needs
exactly the same thing, and everyone will write it slightly differently and
slightly wrong. A hook — "given a prefix and a function from row to expected
derived keys, rebuild and report differences" — would be broadly reusable and
would pair naturally with `PREFIX.DIGEST`.

### R5. Publish v4.0.0, or state the 3.x support line

`Cargo.toml` says `4.0.0` but the latest tag is `v3.18.0` and 4.0 lives on
`feature/v4`. We pinned 3.18 because putting a business system's primary store
on an unreleased branch is not defensible — and because mailrs runs 3.18, so
two products on one version means findings cross-check.

Knowing whether 3.18 is the supported line, or whether we should plan a 4.0
upgrade, would help us schedule. Both D1 and D2 are present in both versions,
so this is not urgent for correctness — only for planning.

### R6. Document the "one row, many derived keys" pattern end to end

The strongest thing we built is making derived state a pure function of the
row: given a row, compute its collection memberships and its uniqueness claims.
Writes diff old against new, so a renamed value releases its old claim
automatically; deletes emit the removals; verification rebuilds from rows and
diffs.

That single idea resolved cascades, uniqueness, and drift detection at once,
and it took us a day to arrive at. It is latent in the cookbook across §2, §5,
§10 and §12 but never stated as one pattern. Written up as a single recipe it
would be the most valuable page in the migration docs.

---

## What we are doing regardless

Migrating. 15 MB, one process, and a system that is not yet the system of
record for finance — the risk profile absorbs D1 and D2 with the mitigations
above, and the operational simplification of three containers becoming one
binary is worth real money to a company this size.

We are happy to re-run any of the measurements above against a fix, and to
contribute the plan-combinator and boot-verification patterns back as cookbook
recipes if they are useful.

Harness for D1/D2 is a ~60-line binary depending only on
`kevy-embedded = "3.18"`; we can send it on request.
