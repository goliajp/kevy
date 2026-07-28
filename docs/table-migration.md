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

### 5. Deleting the old structure: readers first, writers second

When the shadow window closes, remove the old index's **readers
first**, then its writers. The tempting opposite order has a silent
failure mode: a missing key reads as 0 or empty, not as an error — a
reader left behind after the writers are gone serves quietly wrong
answers instead of crashing. Then delete the stored keys.

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

## See also

- [tables.md](tables.md) — the declaration surface, VERIFY semantics,
  composite ORDERPATH rules.
- [cookbook.md](cookbook.md) — sequences, constraints, composite
  ordering as recipes.
- [tiering.md](tiering.md) — indexes stay hot while rows go cold;
  index-only queries touch zero rows.
