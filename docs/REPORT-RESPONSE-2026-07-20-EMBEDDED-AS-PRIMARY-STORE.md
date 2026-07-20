# Response — embedded as a primary store

Answers every item in
`REPORT-FROM-GOLIAJP-2026-07-20-EMBEDDED-AS-PRIMARY-STORE.md`. The two
defects are fixed, four of six requests are implemented, and the parts
that are not are named as such rather than folded into a summary.

The report was unusually precise — separating what you verified from
what you derived, and giving reproductions — and one sentence in it
caught a wrong fix of mine before it reached you. That is called out
where it happened.

**Everything below is on `feature/v4` and none of it is released.**
Release timing is R5.

---

## Part 1 — the defects

### D1 / R1a — a rejected `atomic()` left its writes live · **fixed**

Confirmed and reproduced with your program before anything changed.
Rejection now **rolls back**: each key is snapshotted on first touch
(value plus remaining TTL) and restored in reverse on `Err`.

Not implemented as you suggested, and the difference is worth knowing
for review. Buffering mutations and applying at commit would mean
reimplementing 24 `AtomicCtx` methods across strings, hashes, sets,
lists and zsets as an overlay — five families, five chances to get one
wrong. Snapshot-and-restore puts all five value types on one path, so
hashes cannot roll back correctly while zsets quietly do not. The shard
write lock is held for the whole closure, so nothing observes the
intermediate state and the two designs are externally equivalent.

One choice to review: the keys to snapshot are **declared by each
mutating method**, not recovered from the queued AOF argv. Recovering
them would need `DEL k1 k2` treated differently from `SET k v`, and a
mistake there means "restoring" a key that merely equals some value
byte-string — deleting untouched data. Same defect class as yours,
pointed at a different victim.

`atomic_all_shards()` had the identical shape and is fixed too. A
rejected transaction there was diverging several shards at once.

Five tests: overwrite rolls back, a created key is removed, all five
value types restore, a repeatedly-written key lands on its
pre-transaction value rather than an intermediate one, `Ok` still
commits.

### D2 / R1b — `atomic()` was not crash-atomic · **fixed, on the second attempt**

My first answer to this was wrong, and your report is what caught it. I
wired group commit, wrote that there was "no window in which half a
transaction is durable", and that claim was false when I made it. Your
line:

> `kill -9` leaves everything already handed to `write()` in the page
> cache, so it persists regardless of fsync policy. **The partial
> transaction comes from the loop shape, not from fsync.**

Exactly right. The AOF writes through a 256 KiB buffer, so frames reach
the kernel whenever it fills. Measured, group commit only:

```
n=20000 (~760 KB of frames, 3x the buffer):  kill@12ms -> 6393/20000
```

A durable half-transaction. My "fix" bought atomicity only for
transactions that happened to fit in the write buffer, with an
undocumented cliff at 256 KiB — for someone choosing this engine for
payroll on the strength of that sentence, worse than no guarantee.

Fixed properly with **transaction markers in the log**. `begin_group`
writes a begin marker, `end_group` a commit marker; replay buffers every
frame after a begin and applies the batch only on the matching commit,
discarding it at EOF. "Did this transaction finish" becomes a property
of the log, independent of how much was flushed. The markers ride as
ordinary v2 records whose name starts with NUL — no format change, no
collision with a RESP verb.

Re-measured with your harness:

```
n=20000  (3x buffer, 10 samples) : only 0/20000 or 20000/20000
n=100000 (15x buffer, 12 samples): only 0/100000 or 100000/100000
```

**Verification status, precisely:** crash-verified, not source-verified —
`kill -9` at swept offsets at 3× and 15× the write buffer. **Not**
power-loss verified: `kill -9` leaves the page cache intact, so these
exercise process death, not media loss. Under `Fsync::Always` the commit
marker is inside the synced run, so power loss should behave the same. I
have not tested it and do not claim it.

### On your self-imposed discipline

You wrote that you validate before writing inside every atomic closure,
and called it "a convention, not a guarantee — one refactor away from
being violated silently". **You should be able to drop it.** Five tests
pin the rollback contract; if any regress, CI fails. Keeping it costs
nothing, but it should no longer be load-bearing for your range-overlap
constraint.

---

## Part 2 — the requests

### R2 — collection reads inside `atomic` · **done**

`SMEMBERS`, `SISMEMBER`, `LRANGE`, `LLEN`, `SCARD`, `ZRANGEBYSCORE`, on
both contexts. They hold the shard write lock already, so there was
never a consistency reason to withhold them — the omission is what
pushed you to model child collections as hashes.

A parity test now checks the two contexts expose the same surface; they
had drifted before.

### R3 / F2 — index reads inside a transaction · **done, with a restriction you should push back on if it hurts**

Implemented on `atomic_all_shards` only. You asked for the restricted
single-shard form, and **the restriction is the part that cannot be
shipped.**

An index entry lives on the shard of the key it indexes, so "does any
row have this email" is a question about every shard. `atomic()` holds
one lock and could answer only for its own slice — a uniqueness check
that consults 1/N of the keyspace returns "unique" nearly always. That
is not a weaker version of the feature, it is a wrong one, so it is
absent rather than present with a footnote. `atomic_all_shards` already
holds every write lock, so the same read is complete and consistent.

```rust
if ctx.idx_count(b"email_idx", &want, &want)? > 0 { return Err(Taken); }
```

Second limit, with a test on it: **these see committed state, not the
transaction's own writes.** Index maintenance runs at commit. A closure
inserting two rows must compare them to each other itself.

If holding all shard locks is too coarse for your write path, say so —
that is a real cost and there may be a middle design, but it needs to
start from what your transactions actually contend on.

### R4 — boot-time reconciliation hook · **done**

`store.snapshot().reconcile(rows_prefix, derived_prefixes, derived)`,
taking the same row→derived-keys function your write path uses.

Two things a hand-rolled version gets wrong, handled by construction:

- **It runs against a frozen snapshot**, taken under every shard lock, so
  a concurrent write is not reported as drift. A live scan cries wolf in
  exactly the situation a boot check can least investigate.
- **It diffs both directions.** A claim whose row is gone is an *orphan*,
  not an absence — what a half-applied update leaves behind, and what
  silently blocks a later insert. A missing-only checker reports "clean"
  during the failure it exists to catch.

`derived_prefixes` is required, not defaulted: a derived key outside it
is invisible to both directions, which is the one way to get a falsely
clean report.

Counts are exact; example lists cap at 1000 with `truncated()` saying so.

Note what this asks of the writer: the snapshot cannot hide a writer
that leaves a row and its claim half-applied, so the pair must go in one
transaction. Reconciliation and atomic writes are the same guarantee
seen from two ends — which is also why D2 mattered so much to this.

### R5 — publish 4.0.0 or state the support line · **answered, and there is a question back for you**

See `SUPPORT-LINE-3X-VS-4X-2026-07-20.md`. Short version: **D1 and R2 can
reach 3.18.x; D2 cannot** — the markers ride as AOF v2 records and 3.18
writes v1 with no envelope. That is a format dependency, not a schedule.

A 3.18.x with group commit alone would give you "crash-atomic for
transactions under 256 KiB, and silently not beyond" — the shape I
shipped by mistake and will not ship deliberately.

**The question only you can answer:** if that shape is still useful —
your transactions are small, and partial crash-atomicity now beats none
while you migrate — a 3.18.x becomes worth building. You know your
transaction sizes. Tell me and I will build it; otherwise the default is
not to.

### R6 — document the pattern end to end · **done**

`cookbook.md` §21, "Derived state as a pure function of the row". You
were right that it was latent across §2, §5, §10 and §12 and never
stated once. The write-up leads with the diff table, because the update
row is what pays: hand-written cascade code adds the new claim and
forgets to release the old one, since release is the case nobody
demonstrates in a ticket.

§10 now points at §21 rather than repeating a special case of it.

---

## Part 3 — the findings

| | Status |
|---|---|
| **F1** — `AtomicCtx` omissions drove your data model | Closed by R2 + R3. The 22-verb surface was an accident of what got added first, not a design. |
| **F2** — no index read in a transaction | R3. |
| **F3** — `MAX_INDEXES = 64` global, budget model implicit | `indexes.md` now has a **The index budget** section: 64 is global, parent-child navigation belongs in link keys and costs nothing, spend slots only on global ranges, text and aggregates. Your "58 tables looks impossible, actually needs ~19" is exactly the story it exists to prevent. |
| **F4** — composite group keys for `KIND agg` | Documented under `KIND agg`: move the condition into the group key, materialise the composite at write time, split in the app. Including that conditional aggregates work the same way. |
| **F5** — `Fsync::Always` is a trap | Half-moot and half-fixed. `persistence.md` now states plainly that `AppendFsync` governs the power-loss window for individual commands and has never governed block atomicity. With D2 fixed, `Always` is an honest choice again — it costs what it costs and buys what it says. Your reasoning was sound; the setting was misnamed for what you needed. |

---

## What is still open

- **Nothing here is released.** See R5.
- **Power-loss verification** of D2 is not done, only process death.
- **The 3.18.x question above** is genuinely yours; I am not deciding it
  by default.

Re-verification against these fixes is welcome, particularly the
range-overlap constraint — it exercises a longer closure than the
reproduction does, and it is the one I would most want a second pair of
eyes on.
