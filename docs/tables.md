# Tables (`TABLE.*` / `table_*`)

A table is a **named, verifiable declaration** that compiles — at
declare time — into the index and view primitives kevy already has.
`TABLE.DECLARE` takes a prefix, typed columns, secondary indexes and
composite sort paths, and emits ordinary named indexes; nothing new
runs at query time. This is the engine's standing rule (Law 3) in
user terms: **kevy never plans a query — you name your access paths**,
and a table is the ergonomic way to name a whole family of them at
once.

```
TABLE.DECLARE user PREFIX u: PK id
    COLUMN id str COLUMN name str COLUMN age i64
    COLUMN dept str COLUMN email str
    INDEX age range VALUES dept name
    INDEX email unique
    ORDERPATH by_dept_age ON dept THEN age DESC

IDX.QUERY user.by_dept_age WHERE dept EQ eng LIMIT 20
```

Rows are hash keys under the prefix, exactly as before — declaring a
table changes nothing about how you write (`HSET u:1 name alice age
30 …`) and imposes **no schema**: a row missing a declared column is
a row where that column is NULL (the absent-field semantics every
index already has). The declaration buys you compiled access paths, a
`VERIFY` surface, and one-verb lifecycle for all of them.

> **Migrating from hand-maintained indexes?** Read
> [table-migration.md](table-migration.md) first — eight
> production-paid lessons, and the measured drift numbers (89 %
> never-written, 76 % never-removed) that are the reason tables
> exist.

> **Declaration never panics.** `TABLE.DECLARE` / `Store::table_declare`
> answer every invalid spec — unknown columns, colliding names, missing
> PK, anything — with a named error, and a refused declare installs
> nothing. This is a hard guarantee, enforced by `compile_table`
> validating for itself and fuzzed continuously (`table_spec`): a bad
> spec on your boot path is a log line, not a restart loop.

## The declaration model

`TABLE.DECLARE` compiles each clause into a named index:

| clause | compiles to |
|---|---|
| `INDEX <col> range\|unique [VALUES <col>…]` | a scalar index named `<table>.<col>` over the prefix, with the `VALUES` columns stored per row (typed from the column declarations) |
| `ORDERPATH <name> ON <col> [DESC] [THEN <col> [DESC]]…` | a composite range index named `<table>.<orderpath>` — one order-preserving byte key per row |

The compiled names share one namespace — `<table>.<col>` vs
`<table>.<orderpath>` — so an ORDERPATH named like an indexed column
is refused at declare time, by name. The compilation is a single
implementation shared by the server and the embedded store (the
dispatch oracle byte-compares the two faces in CI), and it is
**atomic**: on any error nothing installs — no half-declared table.

Everything a compiled index does is what a hand-declared `IDX.CREATE`
does: same backfill behavior, same `-INDEXBUILDING` discipline, same
sidecar persistence, same budget refusal
([indexes.md](indexes.md)). `TABLE.DROP` drops the table and every
index it compiled.

## Grammar

```
TABLE.DECLARE name PREFIX p PK col
    COLUMN name i64|f64|str [COLUMN ...]
    [INDEX col range|unique [VALUES col ...]] ...
    [ORDERPATH name ON col [DESC] [THEN col [DESC]] ...] ...
    [WINDOW col SPAN n BUCKET n]
    [AUTODECLARE n]    # let the engine add up to n paths — see below
TABLE.ENSURE ...       # TABLE.DECLARE's boot form — see below
TABLE.REPLACE ...      # explicit drop + declare + rebuild
TABLE.DROP name        # drops the table + its compiled indexes; 1|0
TABLE.LIST             # name/prefix/pk + counts + the window (or -)
TABLE.VERIFY name      # component fsck + a bounded column spot check
```

## The sliding window

`WINDOW <col> SPAN <n> BUCKET <n>` declares a sliding hot window over
an `i64` column. `SPAN` and `BUCKET` are plain integers **in the
column's own units** — the engine never assumes a time base, so the
column can be epoch seconds, epoch millis, a sequence number, anything
monotone with data age. The boundary will advance in whole buckets;
an evicted bucket becomes one cold segment.

Two declare-time refusals, both named:

- the window column must be a declared `i64` column;
- it must have an access path whose tree tail answers `max(col)` for
  free — a single-column `INDEX <col>` on it, or an `ORDERPATH` whose
  *first* column is it, ascending. Without one the declaration is
  refused (`WINDOW needs an access path on '<col>'`) — which also
  guarantees every windowed table can serve cross-window range queries
  through that same path.

What slides today: the **single-column INDEX on the window column**.
As the boundary advances (whole buckets), the index tree's
out-of-window prefix moves into immutable cold segment files under
`segs-<shard>/` — index memory shrinks while plain `RANGE` / `COUNT`
queries keep answering over hot + cold **byte-identically** to an
unwindowed index (rewrites, deletes and revivals of cold rows
included; the semantic-equivalence e2e pins this). Cold index segments
are derived spill, not truth: rows stay ordinary hashes in the hot
keyspace, and a restart simply rebuilds and re-slides.

Two edges, both explicit: clause-carrying queries (`FILTER` / `SORT` /
`DISTINCT` / `FACET`) on an index that has cold segments refuse by
name until the claused cold path lands — never a silently incomplete
answer; and an `ORDERPATH` led by the window column satisfies the
declaration but does not slide yet. Memory-only deployments (no data
dir) accept the declaration and simply stay all-hot.

## The boot pattern: `ensure`

Declaring at boot is the steady state — the schema lives in your
code, and every process start states it. `TABLE.ENSURE` (embedded:
`Store::table_ensure`, returning `TableEnsure::{Created, Unchanged}`)
takes the same grammar as `TABLE.DECLARE` and is its idempotent form:

- **No such table** → declared and built: `Created`.
- **Identical spec** → a no-op success: `Unchanged` (`+UNCHANGED` on
  the wire). Every later boot takes this path.
- **A different spec** → a **named refusal that says what changed**
  (`COLUMNS`, `INDEXES`, `ORDERPATHS`, `PREFIX`, `PK`) — never a
  silent rebuild. A rebuild is a full backfill over the prefix
  domain; something that expensive must be asked for by name:
  `TABLE.REPLACE` (embedded: `table_replace`) is that ask — explicit
  drop + declare + rebuild, validating the new spec **before**
  dropping the old table, so a bad replacement leaves the old table
  serving.

Plain `TABLE.DECLARE` remains the strict form: re-declaring an
existing name is an error. Use `ensure` at boot, `replace` in
migrations, `declare` when a duplicate name means a bug.

- Column types are `i64 | f64 | str` — the scalar index types.
  Everything else (timestamps, booleans, enums) is app-encoded into
  one of the three, and the coarse mapping is stated rather than
  hidden (kevy-sql prints a note per coerced column).
- `PK` names a declared column; it is documentation plus a `VERIFY`
  surface — rows are addressed by their key, exactly as today.
  `serial`-style id allocation is a recipe
  ([the sequences recipe](cookbook.md#3-sequences)), not an engine
  feature.
- Up to 64 tables; every structural refusal is named (duplicate
  column, unknown `VALUES` column, name collisions, …), never
  silent.

`TABLE.VERIFY` recomputes **every counter fresh, at the moment of the
call, in both directions** (4.1 — before, `coerce_failures` was a
lifetime tally that also swallowed absent columns, so it could not be
read against the fresh `drift` beside it):

- **index→row**: `entries` / `bytes` / `duplicates` / `drift` /
  `checked` — every held entry re-derived from its row.
- **row→index**: every prefix row classified by cause — `rows`
  walked, `coerce_failures` (present but fails to coerce), `excluded`
  (a composite `str` component over 255 bytes), `absent` (a missing
  component column: NULL by design, not an error), and **`missing`**
  (the row derives a value yet has no entry — the writer that forgot
  this table exists, the one class a drift walk structurally cannot
  see).

`entries` = `rows − excluded − absent − coerce_failures` when nothing
is wrong; each exclusion cause now has its own name instead of
surfacing as an unexplained entries diff. A bounded spot check (up to
64 sampled rows per shard) additionally asserts every *present*
declared column coerces to its declared type. It answers
`-INDEXBUILDING` while any component index is still backfilling.

## Composite ORDERPATH semantics

An ORDERPATH mechanizes [the composite-ordering recipe](cookbook.md#8-composite-ordering-order-by-a-b)
— the `ORDER BY a, b DESC` walk — into a real composite index: one
order-preserving byte string per row, so a single B-tree answers the
query the way a relational composite index does. The rules:

- **`WHERE` takes a leading prefix.** `WHERE a EQ x [b EQ y …]
  [RANGE c min max]` must name the composite's columns in declared
  order from the front: an equality prefix, then at most one range on
  the *next* column; everything after is unconstrained (classic
  composite-B-tree semantics). Naming a non-prefix column is a named
  error — never a scan.
- `RANGE` is terminal within `WHERE` — nothing may follow it, because
  nothing after a range is representable as one contiguous walk.
- **`DESC` per component** is honored in the stored order, so
  `ON dept THEN age DESC` pages each department's rows from the
  oldest-largest end with no re-sort.
- **A row missing any component column is excluded** from the
  composite index (same for a coerce failure) — it remains fully
  visible through every other access path. A `str` component longer
  than **255 bytes** also excludes the row: the same class of cap a
  relational B-tree puts on index-row size, and what keeps range
  bounds exact. Up to 8 components.
- `IDX.COUNT` applies `FILTER` on stored `VALUES` columns — counting
  a filtered axis (the badge/tab number) is one call, not a paged
  fetch-and-len.
- `WHERE` works on `IDX.COUNT` too, and on an index that declares no
  composite columns it is refused by name.
- **A non-zero `duplicates` in `TABLE.VERIFY` means the ORDERPATH is
  not a total order** — rows tying on every component collapse to one
  entry, and cursor pagination will skip or repeat at the tie
  boundary. End the composite with a **bounded** tie-break column
  (numeric id, or a fixed-width hash of the natural key) — not a raw
  unbounded string like a Message-ID, which walks into the 255-byte
  exclusion cap instead of breaking the tie.

```
IDX.QUERY user.by_dept_age WHERE dept EQ eng                  # all eng, age DESC
IDX.QUERY user.by_dept_age WHERE dept EQ eng RANGE age 31 46  # eng, 31<=age<=46
```

## Querying tables

Queries stay `IDX.QUERY` against the compiled names — a table adds no
query verb, because the engine evaluates nothing at query time:

```
IDX.QUERY user.age RANGE 25 45                          # driving range
IDX.QUERY user.email EQ d@x                             # unique point lookup
IDX.QUERY user.age RANGE 0 100
    FILTER dept EQ eng SORT name ASC LIMIT 20 OFFSET 20 # clauses on VALUES
IDX.QUERY user.age RANGE 0 100 FACET dept
IDX.QUERY user.by_dept_age WHERE dept EQ eng LIMIT 20 FIELDS name email
```

`FILTER` / `SORT` / `DISTINCT` / `FACET` / `OFFSET` read the columns
the index **stored at `VALUES` declaration time** — the same clause
grammar and the same exact-across-shards semantics as their text
originals ([text-search.md](text-search.md)): `FILTER` applies before
the page so a qualifying row ranked deep still reaches `LIMIT`,
`FACET` counts the whole match set, missing values sort last both
ways. Naming a field the index did not store is an error that names
the fields it did. The driving predicate is always the indexed
range/EQ/WHERE — there is no `WHERE`-without-an-index.

`OFFSET` is the one clause here that costs more the bigger it gets, and
**the only surface in this engine that gets slower as you add shards**:
every shard fetches `limit + offset` hits, because no shard can know
which of its own hits survive the global merge, so the origin
materialises `(limit + offset) × shards` to hand back `limit`. Measured
on 30 000 in-range rows returning `LIMIT 20`, `OFFSET 1000` costs
1.53 ms on one shard and 6.90 ms on eight. Page with the returned
cursor — constant per page, and position-stable
([rds-workloads.md](rds-workloads.md#order-by--limit--offset)).

**Index-only queries touch zero rows.** A FILTER/SORT/COUNT query
answers entirely from the RAM-resident index — the row-read counter
is asserted `== 0` in the gate suite (`bench/tablegate.sh`). This is
the tiering synergy the two features were designed around: with
[transparent tiering](tiering.md) on, a fully-cold table serves
index-only queries with **zero disk reads**, and only the final
hydration page (`FIELDS …`) pays cold reads — one per row, batched.
An index without `VALUES` columns is byte-identical in memory and
query path to one on a store that never declares them (the
zero-cost-when-undeclared gate).

## `AUTODECLARE`: the paths you did not write

A query against a column you never indexed is refused by name. That
refusal is also a fact about your workload, and `IDX.ADVISE` shows the
shapes that keep hitting it. `AUTODECLARE n` says: *when a shape has
been refused often enough and it grounds on a column I declared, go
ahead and declare the path for me — up to `n` of them.*

**"Often enough" is 16 refusals of the same shape.** The number is a
constant, not a knob: a per-table threshold would be exactly the
per-workload tuning this engine claims not to need, and a workload
whose shape arrives slowly pays 16 refusals before relief either way.
It is written here because an operator reading `IDX.ADVISE` and
wondering why nothing has happened yet deserves the number, not
because it is something to set.

Everything about it is bounded on purpose:

* **Off unless you ask.** No clause, no loop. This is not a default.
* **Capped by the number you wrote.** Budget spent means the query
  keeps being refused and the shape stays in `IDX.ADVISE` for you to
  read — the engine does not quietly raise its own limit.
* **Only over declared columns.** A shape naming a column the table
  does not declare never grounds; `IDX.ADVISE` still reports it, so
  the answer stays yours.
* **Addition only.** Dropping an index is a human act. The worst case
  of a bad guess is bounded wasted memory, never a lost path.
* **Visible.** Each such index carries an `auto` marker in `IDX.LIST`,
  and the table's spec keeps the ledger — you can always read back
  which paths you wrote and which the engine did.
* **Out of band.** The query that crosses the threshold still gets its
  error. The next one finds the path building. Declaring never happens
  inside a query's answer.

This is not a query planner, and the distinction is the whole point:
the engine never chooses *which path to run* — your query names it.
`AUTODECLARE` only extends the declaration, on your invitation, within
your budget, where you can see it. Query time stays a law: run the
declared path, refuse the rest by name.

## NULL, uniqueness, and what is enforced

- **NULL = absent field.** No column is required; a row missing an
  indexed column is simply not in that index. There are no engine
  `CHECK`s, defaults, or NOT NULL — constraints are recipes
  ([the constraints recipe](cookbook.md#5-check-constraints-and-multi-key-invariants),
  atomic blocks).
- **Uniqueness is verify-not-enforce** at the table layer: a `unique`
  index is the same fence `IDX.CREATE KIND unique` builds
  ([indexes.md](indexes.md#uniqueness-is-a-fence-not-a-lock) — the
  reservation pattern makes it race-free), and `TABLE.VERIFY` reports
  `duplicates` instead of the engine rejecting your write after the
  fact.

## What it is NOT

Stated as refusals, because the engine refuses them by name rather
than approximating: **no runtime SQL** (send `TABLE.DECLARE`, not
`CREATE TABLE`, to the server), **no query-time joins** beyond a
view's `VIA` dereference ([views.md](views.md)), **no HAVING /
subqueries / expressions**, **no engine-enforced constraints**. The
SQL-to-kevy mapping for each of those lives in
[rds-workloads.md](rds-workloads.md), the working recipes in
[cookbook.md](cookbook.md), and the schema-compilation path below.

## kevy-sql: compile a schema, don't send one

`kevy-sql` (and its `kevy-cli sql` face) is a **declaration-time
compiler** — it reads a PG/MySQL-dialect schema file once, like a
migration tool, and emits the explicit declarations:

```console
kevy-cli sql compile schema.sql                          # print the plan
kevy-cli sql compile schema.sql --apply --url 127.0.0.1:6004
```

- `CREATE TABLE` → `TABLE.DECLARE` (types coarsely mapped to
  `i64|f64|str`, each mapping noted honestly).
- `CREATE [UNIQUE] INDEX` → `INDEX` clauses; PG `INCLUDE` covering
  columns → stored `VALUES`; multi-column indexes → an `ORDERPATH`.
- Constant single-table `CREATE VIEW … AS SELECT` → an engine view;
  a parameterized one → a **query card**: a ready-made `IDX.QUERY`
  template with `$N` slots your app fills in.
- The compiler does not plan either: it matches your view against
  the access paths you declared, and when none fits it tells you
  which declaration to add (`add: CREATE INDEX ON t (dept, age)`)
  instead of inventing a scan. Ad-hoc SQL, joins, subqueries, `OR`,
  `GROUP BY` and friends are refused with `line:col` and a pointer
  to the recipe that replaces them.

The end-to-end walkthrough — a real users/orders/order_items schema
compiled, applied and queried — is
[the schema-porting recipe](cookbook.md#22-porting-a-pgmysql-schema).

## Embedded

Typed API, same compilation, no text grammar required in-process. The
declaration types — `TableSpec`, `TableIndex`, `OrderPath` — are
re-exported by the facade (4.1): import everything from
`kevy_embedded`, never from an internal crate.

```rust
use kevy_embedded::{TableEnsure, TableSpec};

match store.table_ensure(spec)? {    // the boot verb: validated,
    TableEnsure::Created => {}       //   compiled, built synchronously
    TableEnsure::Unchanged => {}     //   — or a no-op on a same-spec boot
}
let tables = store.table_list();
let report = store.table_verify_report(b"user")?;  // named fresh counters
assert_eq!(report.per_index[0].missing, 0);        //   + spot check
store.table_drop(b"user");
```

The wire form (`db.cmd("TABLE.DECLARE", …)`) works too and parses
with the identical shared grammar — server/embedded byte parity is
pinned in CI by the dispatch oracle.

## Performance

The gate clamps, and their measurement status stated plainly: the
conformance/parity/refusal/index-only assertions run green in this
tree (`bench/tablegate.sh`). The **throughput clamps** — indexed
point lookup p99 ≤ 1 ms @ 10 M rows, FILTER+SORT+LIMIT-20 page
p95 ≤ 5 ms @ 10 M rows, write tax with 3 indexes + declared
VALUES ≤ 15 % vs bare `HSET` — are perfgate metric lines whose
baselines are **pending the dedicated bench box**
(`bench/capacity-envelope.sh` records them). Until
recorded there, they are targets, not measurements — this page will
not quote them as results.

Write cost is the standard index tax: one field read plus one segment
update per compiled index per matching write; an empty catalog costs
one untaken branch.

## See also

- [indexes.md](indexes.md) — the index engine tables compile into.
- [tiering.md](tiering.md) — the companion feature: indexes hot,
  rows cold.
- [rds-workloads.md](rds-workloads.md) — the full SQL-vocabulary
  mapping (what compiles, what is a recipe, what is refused).
- [cookbook.md](cookbook.md) — the composite-ordering and
  schema-porting recipes.
- [views.md](views.md) — named compositions over the same indexes.
