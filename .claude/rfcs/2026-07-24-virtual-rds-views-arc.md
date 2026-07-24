# RFC — virtual RDS views: serving PG/MySQL-shaped business logic on kevy

**Date:** 2026-07-24 · **Status:** DESIGN ROUND — user vision received
("我们需要在索引能力上做好充分的设计，要用虚拟 rds 视图可以兼容 pg / mysql
等业务"), this RFC turns it into a lawful, staged path. **Nothing here is
implementation-started**; the trains and the open decisions at the end are
the 拍板 surface.

Constraint set by the user: this is an ADDITIVE capability — it must not
affect mainline performance or logic. Constraint set by the constitution:
Law 3 (meaning and planning never enter the engine) is locked, and the
REFUSED table (SQL/query DSL, planner/statistics, query-time joins,
engine-enforced schema, query-time aggregation) stands. This RFC's central
claim: **the user's goal is reachable WITHOUT amending Law 3** — by
compiling relational declarations into the existing engine genres at
declaration time, and by filling four concrete engine gaps that are all
Law-2-legitimate (declarative at write time, explicitly named access
paths).

## 1. What "兼容 PG/MySQL 业务" decomposes into

A business app on PG/MySQL uses, in practice-frequency order:

| # | Relational need | kevy today | Gap |
|---|-----------------|------------|-----|
| R1 | Table + typed columns + PK | hash-per-prefix, cookbook §1 (manual) | No declaration object; conventions live in app code |
| R2 | Secondary index lookups (WHERE col = / range) | IDX Range/Unique, EQ/RANGE | ✅ exists |
| R3 | Multi-condition WHERE (AND/OR over indexed cols) | IDX.QUERY COMPOSE (2 legs), VIEW tree (≤4 leaves) | Mostly exists; >2-leg ad-hoc AND is view-shaped |
| R4 | **WHERE on non-driving columns (residual filter)** | FILTER — **text kind only** (catalog.rs:343 refuses VALUES elsewhere) | **Engine gap G1** |
| R5 | **ORDER BY col [DESC] (incl. multi-column)** | SORT — text-only; view order_by = single index; composite = manual score encoding (cookbook §8) | **Engine gap G1 + declaration-layer automation** |
| R6 | LIMIT/OFFSET pagination | LIMIT+CURSOR (scalar), LIMIT+OFFSET (text only) | **Engine gap G2** (OFFSET on scalar/view) |
| R7 | COUNT / GROUP BY aggregates | IDX.COUNT, Agg kind (write-time maintained) | ✅ exists (Law-3-shaped: write-time, not query-time) |
| R8 | Lookup joins (FK deref, N+1-free) | Via (≤2 hops, pure template, no target predicates) | ✅ exists, fenced — the permitted maximum |
| R9 | General joins / HAVING / subqueries | — | ❌ REFUSED (Law 3) — stays app-side; state honestly |
| R10 | Transactions (multi-row invariants, CAS) | atomic()/atomic_all_shards + index reads | ✅ exists (cookbook §4/§5/§21) |
| R11 | Uniqueness, sequences, soft delete, cascades | Unique kind, INCR blocks, flag+view, CDC | ✅ cookbook recipes |
| R12 | SQL text / PG-MySQL wire protocol | — | ❌ in-engine REFUSED; see §5 for the out-of-engine option |

Reading: **most of the RDS surface already exists** (that is what the v3
serving-engine arc built). What is missing is (a) a **declaration object**
that turns "a table" from an app-side convention into a named, verifiable,
catalog-managed thing, and (b) **four engine gaps** — all of them
generalizations of machinery the FTS arc already built for text.

## 2. The design — three layers, only one touches the engine

### Layer A — engine: generalize the doc-values machinery (gaps G1/G2)

The FTS arc built per-document stored VALUES + FILTER/SORT/DISTINCT/FACET
+ OFFSET — but only on the text kind. The relational workload needs
exactly these on **scalar (Range/Unique) queries and views**:

- **G1 — `VALUES` on Range/Unique kinds** (lift catalog.rs:343): a range
  index may declare stored columns; `IDX.QUERY name RANGE … FILTER col EQ v
  SORT col2 DESC DISTINCT col3 FACET col4` then works exactly as it does
  for MATCH — same clause grammar, same ValueTest, same "predicates are
  declared at write time" law-compliance (the stored column is declared in
  the spec; FILTER only tests declared columns; there is still no
  WHERE-without-an-index — the driving predicate remains the indexed
  range/EQ).
- **G2 — `OFFSET` on scalar/compose/view queries** (text already has it):
  merged-rank skip, the FTS arc solved the cross-shard semantics
  (pass-2 takes limit+offset per shard, origin drains).
- **G3 — multi-column ORDER BY in views**: not an engine change if done at
  the declaration layer (Layer B encodes composite sort keys into one
  derived index, cookbook §8 automated). Engine change only if we later
  want true tuple-ordered indexes; NOT in this arc.
- **G4 — view-level FILTER…?** NO. View-level predicates are permanently
  refused (views-design §4). The lawful equivalent: a view leaf references
  an index whose *stored values* carry the filter columns, and FILTER
  clauses ride `VIEW.QUERY` the same way they ride `IDX.QUERY` — testing
  **declared stored columns of the component indexes**, not arbitrary row
  fields. This needs a constitution-consistency review in its own design
  round before implementation (it is the one point where "FILTER on views"
  brushes against the refusal; the distinction — declared write-time
  columns vs query-time row evaluation — must be written down and 拍板'd).

Perf guardrail: all of G1/G2 are opt-in per index spec (an index without
VALUES stores nothing, pays nothing — the same physical-bypass pattern as
positions). textgate/perfgate lines extend to the scalar-FILTER path.

### Layer B — declaration: the `TABLE.*` namespace (new, engine-adjacent)

A catalog object that compiles to existing genres — no new query
machinery, no meaning in the engine:

```
TABLE.DECLARE user PREFIX user: PK id
  COLUMN name str  COLUMN email str  COLUMN age i64  COLUMN dept str
  INDEX email UNIQUE
  INDEX age RANGE VALUES dept name      ← stored columns for FILTER/SORT
  INDEX dept RANGE
ORDERPATH recent_by_dept ON dept SORT age DESC   ← compiles to a derived
                                                    composite-score index
                                                    (cookbook §8, automated)
```

What it does at DECLARE time (all existing primitives): creates the
IDX.CREATE specs; creates named views for declared query paths; records
the table in a sidecar catalog (same lifecycle genre as index/view
catalogs: TABLE.LIST / TABLE.VERIFY / TABLE.DROP; verify = the component
indexes' verify plus column-type spot checks). What it does NOT do:
enforce schema on writes (Law 3 — a row with a missing column is a row
with an absent field, exactly today's NULL semantics), evaluate anything
at query time, or choose access paths (queries still name their index or
view explicitly).

Why a TABLE object at all, if it compiles away? (1) it moves the modeling
conventions (cookbook §1) from every consumer's app code into one named,
verifiable declaration — the goliajp report showed consumers rebuilding
exactly this by hand; (2) it is the compilation target the SQL translator
(Layer C) needs; (3) TABLE.VERIFY gives migration/驱动 drift a fsck.

### Layer C — out-of-engine: the SQL declaration compiler (`kevy-sql`)

Where "兼容 pg / mysql" becomes concrete without breaking Law 3:

- A **separate tool** (kevy-cli subcommand or standalone crate; NOT linked
  into the server): parses a declared SQL subset **once, at declaration
  time** — `CREATE TABLE` → `TABLE.DECLARE`; `CREATE VIEW … AS SELECT
  single-table WHERE indexed-cols AND filters ORDER BY … LIMIT` → the
  IDX/VIEW compilation. Runtime queries are the compiled named views with
  partition arguments (`VIEW.QUERY orders_by_user EQ {user_id} …`).
- Per-query ad-hoc SQL: **stays refused**. The app binds to named paths —
  which is also what a well-run PG app does (prepared statements against
  known indexes); the difference is kevy makes the unplanned path
  impossible instead of slow.
- **PG/MySQL wire emulation (a server speaking the pg protocol): NOT in
  this arc.** It would require per-query SQL → plan translation inside a
  serving process = Law 3's exact red line. If the user wants it later it
  is a separate proxy product built ON the compiler (translate + cache
  compiled plans for a closed statement set), and its own decision.

## 3. Trains (linear, each five-axis gated)

1. **T1 — G1 scalar VALUES/FILTER/SORT/DISTINCT/FACET** (engine,
   kevy-index + query surface; the largest train). Zero-cost-off proof +
   perfgate line for the no-VALUES path.
2. **T2 — G2 OFFSET on scalar/compose/view.**
3. **T3 — TABLE.* declaration namespace** (catalog + compile-to-IDX/VIEW +
   verify/fsck + sidecar; server + embedded parity via the same
   dispatch-oracle discipline that just caught IDX.CREATE drift).
4. **T4 — ORDERPATH composite-sort automation** (declaration-layer
   score-encoding; cookbook §8 mechanized).
5. **T5 — kevy-sql declaration compiler** (out-of-engine tool + docs:
   "porting a PG/MySQL schema" cookbook chapter with a real schema).
6. **T6 — view-FILTER constitution review** (design round only → 拍板,
   then maybe a T7).

## 4. What we will honestly tell consumers it is NOT

No ad-hoc SQL at runtime; no query-time joins beyond Via lookup; no
HAVING/subqueries/window functions; no engine-enforced constraints (CHECK
is the atomic-block recipe; uniqueness is verify-not-enforce). The pitch
is "your PG schema's *access paths*, compiled to explicit indexes and
views, with relational read ergonomics (filter/sort/paginate) at kevy
speed" — not "a drop-in PG".

## 5. Open decisions (拍板 surface)

1. Arc ordering vs the tiering arc (they are independent; tiering RFC is
   the sibling `2026-07-24-tiered-storage-arc.md`).
2. Layer C scope: kevy-cli subcommand vs standalone crate; which SQL
   subset first (CREATE TABLE + single-table CREATE VIEW is the proposal).
3. The G4/T6 question (FILTER on views over declared stored columns) —
   needs its own constitution note before any code.
4. Namespace name: `TABLE.*` vs `SCHEMA.*` vs `RDS.*` (proposal: TABLE).
5. Whether TABLE.DECLARE also emits FEED/CDC wiring options (audit recipe
   automation) in T3 or later.
