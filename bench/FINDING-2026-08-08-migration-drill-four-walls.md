# The migration drill's four walls — every one hit inside the first hour

V2 train (plan: `.claude/plans/2026-08-08-v5-v2-migration-drill.md`).
The charter asked for the pg_dump → plan → apply → move → reconcile →
verify chain against a REAL PostgreSQL, walls finding-ized and fixed.
The drill's own 52k-row seed database produced four, in strictly
increasing depth: two in the compiler's assumptions, one in the
compiler's dialect, one in the ENGINE's wire protocol. None of the
five migration tools themselves failed.

## Wall 1 — NOT NULL / DEFAULT were fatal refusals

Every real pg_dump writes `NOT NULL` on nearly every column. The
compiler refused it as a teaching error, which walled migration day at
the first CREATE TABLE. Fixed: both compile now, each carried as an
honest-mapping note (the unenforceability is still said — once per
column — just no longer fatal). `DEFAULT <expr>` is consumed without
interpretation; the note says to write the default app-side.

## Wall 2 — one refused type killed the whole plan

`billing.amount money` was a PARSE-time fatal error, so the other
three tables lost their plan with it. The plan face's own charter is
"report every fate"; that now extends to DDL: the type verdict moved
from parse to build, `plan()` grew a lenient path where an
undeclarable table becomes a named dropped row (`✗ billing: type
'money' is not in the compilable subset …`) and views over it point
at the drop. `compile()` keeps fatal semantics — it emits commands
about to be applied.

## Wall 3 — the pg_dump dialect

A real `pg_dump --schema-only` is not a hand-written schema: psql
`\restrict` meta lines (new in PG 18), the SET / set_config preamble,
`OWNER TO`, `public.` qualification, `USING btree`, `timestamp
without time zone` — and the load-bearing one, **no inline PRIMARY
KEYs anywhere**: every PK arrives as `ALTER TABLE ONLY … ADD
CONSTRAINT`. Without folding those back, every dumped table refuses
for "no PRIMARY KEY". All implemented in `kevy-sql`'s `parse_dump`
module; FOREIGN KEY / CHECK constraints become notes (the wall-1
precedent), other ALTER forms keep the teaching refusal. Bonus
inconsistency fixed: `timestamptz` compiled while its long spelling
refused.

## Wall 4 — the engine answered redis-cli --pipe with a phantom error

The deepest one, and the only engine defect. `redis-cli --pipe` ends
every stream with a bare CRLF before its ECHO sentinel (inline-safety
padding in redis-cli's own source). Redis consumes an empty inline
line silently; kevy answered `ERR empty command` — so EVERY pipe
import against kevy reported `errors: 1` while losing zero rows.
Frame-bisecting the 52,500-frame stream landed on frame 1, i.e. any
frame: the error was the trailer's, not the data's. Fixed in both
parse entries (borrowed + owned): empty parses — bare CRLF, `*-1`,
`*0` — are consumed and parsing continues, exactly Redis's
processInlineBuffer / processMultibulkBuffer behavior. Pinned by a
test carrying the literal --pipe trailer bytes.

## The gate

`bench/migrationgate.sh`: seeds its own postgres:18 container
(deterministic generate_series data), dumps, plans (asserting the
billing refusal is NAMED and three tables stay declarable), applies
the day-2 schema (the operator's edit, doing what the refusals
taught: money → integer cents, inet → text), COPYs to CSV, converts
to RESP frames, imports, reconciles row counts (52,500) and sampled
fields against psql, requires TABLE.VERIFY drift 0, runs doctor.
Everything trap-cleaned; repeatability = two consecutive PASS runs.

## The lesson worth keeping

The five migration tools (plan / backfill-keys / shadow / doctor /
lint) all held. What failed was everything AROUND them — the
assumptions about what real inputs look like. A chain is only drilled
when a real database has been through it; four walls in the first
hour is the argument that the V2 charter line ("真 PG 库全链") was
load-bearing, not ceremony.
