# Migrating hand-maintained indexes to tables

This chapter exists because a production consumer did the full
migration — a mail system moving its hand-rolled secondary indexes
(sorted sets and counter keys maintained by application code) onto
`TABLE.DECLARE` — and came back with lessons that belong to the
engine's documentation, not to their notebook. Every rule below was
paid for; the order is the order you will need them in.

## Lead with why: the engine's index is the only one that can be verified

Before the how, the argument. When application code maintains an
index, every writer must remember to maintain it, forever. Nothing
checks. The migration above **measured** what that means in a real,
well-maintained codebase, by comparing the hand-maintained structures
against a freshly built engine index:

- **89 %** of rows were missing from one hand-maintained index — a
  writer added later simply never knew the index existed
  (*never-written* drift).
- **76 %** of entries in another were stale — a delete path removed
  the row but not its index entry (*never-removed* drift).
- A third agreed on membership but not on order — the score formula
  had been changed in one writer and not the other (*score* drift).

None of this was detectable from inside the application, because the
index was the only record of what the index should contain. An
engine-maintained index is different **in kind**: derivation runs on
the write path itself, so there is no writer that can forget — and
`TABLE.VERIFY` recomputes both directions on demand
([tables.md](tables.md)): index→row (`drift`) and row→index
(`missing` — precisely the forgotten-writer class above, visible as a
number). The migration is not a performance project; it is a move
from *unverifiable* to *verifiable*.

## The eight lessons, in order of need

### 1. First question: is every query dimension single-valued on the row?

Look at each query you want an index to answer, and ask whether its
dimension is a single value **on the row**. The answer is usually in
your id-derivation or key-construction code, not in the row itself —
the mail system's "thread per mailbox" looked single-valued until the
id code showed a thread can live in several mailboxes. If a dimension
is multi-valued, no column can carry it: model a **membership row**
per (owner, item) — `member:{owner}:{item}` with the owner, the item
and the sort attributes as columns — and let an ORDERPATH sort that.
Deciding this first prevents re-declaring the whole table later.

### 2. Every writer is load-bearing once reads are served

A derived row is populated by whoever writes it. Before cutting reads
over, **enumerate the writers** — every code path that creates,
mutates, or deletes the underlying entity — and confirm each one
writes the row the table is declared over. The writer that forgets is
the one written before the table existed. (This is exactly the class
`TABLE.VERIFY`'s `missing` counter catches after the fact; the audit
is how you avoid meeting it in production.)

**There is no tool for this one, deliberately.** The store does not
hold the fact you need — "which code paths write this table" is not
recorded anywhere in the data, because writers are code and the engine
sees only writes. What a tool *can* do is catch the consequence:
`kevy-cli shadow` reports a forgotten writer as a row the new path is
missing, before the cutover rather than after. Use the audit to avoid
the surprise and the shadow run to prove you avoided it.

### 3. Backfill from the union of every source that can name an item

Legacy indexes disagree with each other — that is the measured 89 %/
76 % above. Backfilling from any *one* of them inherits its holes,
and `VERIFY` cannot see a row that was never written at all. Build
the backfill key-set from the **union** of every structure that can
name an item (old indexes, the primary keyspace scan, archives), then
write rows from the authoritative record.

### 4. Shadow-read before cutover — compare content **and order**

Serve reads from the old path while computing the new answer beside
it, and compare. Compare the **order** too, not just the membership:
score drift produces identical sets in different orders, and a
paginated UI turns that into user-visible churn. Log the first
divergence with **both sort keys** — that one log line names the
drifting writer immediately.

**`kevy-cli shadow` does this for you.** Give it both commands; it
compares the two orders of row keys they produce and exits non-zero on
any disagreement, so a cutover script can gate on it:

```console
$ kevy-cli shadow -p 6004 \
    --old "ZRANGE old:act 0 -1 WITHSCORES" --old-pairs \
    --new "IDX.QUERY u.act RANGE 0 999 LIMIT 20" --samples 50
shadow: 50 samples, 50 diverged (first at sample 0)
  ORDER differs at position 0:
    old: u:5 (sort 5)
    new: u:1 (sort 10)
```

That pair of sort values is the line the lesson is about. The other
shape it reports is `MISSING` — a row the old path has and the new one
does not, which is lesson 2 arriving early: a writer nobody updated,
seen *before* the cutover instead of by `TABLE.VERIFY` afterwards.

Two things it does not guess. A kevy paged reply (`[cursor, [key,
sort, …]]`) is recognised from its shape, but **member/score pairs look
exactly like a plain list** — pass `--old-pairs` for `WITHSCORES` and
friends, or every score is read as a row key and every sample diverges.
And a single disagreement is a lead, not a verdict: both sides are read
back to back on one connection, so a row written between them shows up
here. Run `--samples n` and read the rate.

### 5. Deleting the old structure: readers first, writers second

When the shadow window closes, remove the old index's **readers
first**, then its writers. The tempting opposite order has a silent
failure mode: a missing key reads as 0 or empty, not as an error — a
reader left behind after the writers are gone serves quietly wrong
answers instead of crashing. Then delete the stored keys.

**No tool here either, and this is the one where a tool would be
actively harmful.** kevy does not track who *reads* a key, so any
"nobody reads this any more" probe would be inferred rather than
observed. This lesson's failure mode is already *quietly reading empty
and calling it fine* — a probe that says "checked, safe to delete"
would dress that failure in a confirmation. Order the removal by hand:
readers first, then writers, then the keys.

### 6. A predicate the index lacks ⇒ another ORDERPATH, never a duplicated column

When a new query needs a predicate the current shape cannot serve,
the reflex from the hand-maintained era is to write the value
somewhere else too — which recreates the two-writers-one-truth
problem the migration just removed. Declare **another ORDERPATH**
(or index) over the same columns instead; the engine derives both
from the same row on the same write.

### 7. Boot with `ensure`

Steady state is [the boot pattern](tables.md#the-boot-pattern-ensure):
`TABLE.ENSURE` at every process start — `Created` on the first boot,
`Unchanged` after, and a **named diff refusal** when the code's spec
no longer matches the store's, which is your signal to run a
deliberate `TABLE.REPLACE` migration rather than have one happen to
you.

### 8. Make `VERIFY` part of operations, not part of the migration

The counters are fresh on every call and cheap enough to run from a
cron or a doctor command: `drift` and `missing` should be zero
forever; `absent` / `excluded` / `coerce_failures` name the rows each
exclusion cause claimed ([tables.md](tables.md) has the exact
semantics, including why non-zero `duplicates` on an ORDERPATH means
your pagination needs a bounded tie-break). The point of the whole
migration is that these numbers *exist*; read them.

**`kevy-cli doctor` is that cron.** It verifies every declared table and
answers with an exit code:

```console
$ kevy-cli doctor -p 6004
  OK       user  (rows 59999 · entries 59999 · absent 0 · excluded 0 · coerce_failures 0)
  WARN     ev    duplicates 1 — paging this path needs a bounded tie-break or pages repeat rows
  BUILDING new   — an index is still backfilling, not a verdict
doctor: 3 table(s) — 0 drifted, 1 warned, 1 still building
```

The mapping is this lesson's own words rather than a new opinion:
`drift` and `missing` non-zero **fail**; `duplicates` **warns**;
`absent` / `excluded` / `coerce_failures` are **reported and never
fail**, because each is a legitimate state and a doctor that went red on
a NULL column would be red forever.

Two deliberate choices. A warning does **not** fail by default —
information that fails a cron stops being read — so `--warn-is-failure`
exists for anyone who wants the stricter contract. And a table whose
index is still backfilling answers `-INDEXBUILDING`, which is **its own
outcome, not a failure**: treating it as one would page someone every
time an index is declared.

## See also

- [tables.md](tables.md) — the declaration surface, VERIFY semantics,
  composite ORDERPATH rules.
- [cookbook.md](cookbook.md) — sequences, constraints, composite
  ordering as recipes.
- [tiering.md](tiering.md) — indexes stay hot while rows go cold;
  index-only queries touch zero rows.
