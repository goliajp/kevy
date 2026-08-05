# AUTODECLARE: not the refused thing, but nobody could tell

Second of the two real drifts the refusal-table audit turned up
(`bench/FINDING-2026-08-05-offset-boundary-drift.md` is the first).
Unlike that one, the charter's claim here turns out to be **true** —
and the finding is what surrounds it.

## The apparent contradiction

`docs/rds-workloads.md:438`:

| Refused | Because | Use instead |
|---|---|---|
| query planner / auto index selection | engine must execute declared paths, not decide | `IDX.EXPLAIN` (diagnostic only) |

Meanwhile `TABLE.DECLARE … AUTODECLARE n` lets the engine watch refused
queries and **create indexes on its own** once a shape has been refused
`AUTODECLARE_AFTER = 16` times. The v5 ledger credits exactly this loop
with closing the "no DBA" promise. Read side by side, one says the
engine must not decide and the other has it deciding.

## What the engine actually does

Read, and pinned by `crates/kevy/tests/idx_advise_e2e.rs`:

| Property | Where | Test |
|---|---|---|
| **Off by default** — `autodeclare: 0`, the loop does nothing | `table.rs:76-80`, `advise.rs:74-76` | — |
| **Opt-in with a budget** the operator writes: `AUTODECLARE n` | `table_wire.rs:138-150` | `…:162` declares `AUTODECLARE 2` |
| **Only over declared columns** — `spec.column_type(suffix)?` refuses to ground a path on a column the operator never declared | `advise.rs:102,115` | `…:255` "ungrounded family withheld" |
| **Budget is hard** — spent budget means the query keeps being refused, and the shape surfaces in `IDX.ADVISE` for a human | `advise.rs:79-81` | `…:201-204` |
| **Visible** — every auto path carries an `auto` marker in `IDX.LIST` and a ledger in the spec (`auto_added`) | `table.rs:81-86` | `…:193` counts exactly 2 |
| **Never drops** — addition only; removal stays a human act | `table.rs:78-79` | — |
| **The refused query stays refused** — declaring happens out of band; the query that crossed the threshold still gets its error, the *next* one finds the path | `cmd_index_reduce/advise.rs:58-63` | `…:185` vs `…:188` |

And the thing the row actually names — **selection** — the engine never
does. A query names its own path (`IDX.QUERY user.by_dept_age …`).
There is no per-query choice to make, so there is no planner to have.

**So the row is literally true.** What kevy refuses is *deciding which
path to run*. What `AUTODECLARE` does is *bounded, opt-in, visible,
addition-only extension of the declaration*, and the query-time law —
execute declared paths, refuse the rest by name — is untouched by it.

That distinction is real, and it is a good one: it is the difference
between an optimiser you cannot predict and a schema change you asked
for, capped, and can read back.

## The actual defect

**Nowhere in the prose docs does that distinction appear — because
nowhere in the prose docs does `AUTODECLARE` appear at all.**

Every mention outside the source is a token inside a generated syntax
line in `docs/verb-reference.md`. Not `docs/tables.md`, which is the
TABLE surface's own page and has a `## What it is NOT` section.
Not `docs/rds-workloads.md`, which is where the boundary is declared.
Not the cookbook.

So a mechanism that can **modify a table's declaration and build
indexes** is, from a user's point of view, an undocumented word in a
grammar. For an engine whose pitch is *the boundary is explicit and
holds*, that is the wrong thing to leave un-said — and it is worse
than the OFFSET drift, because OFFSET was at least written down
somewhere, wrongly.

It also undercuts the differentiation it was built for. The v5 ledger
lists the auto-declaration loop as the mechanism that makes "no DBA"
true. A buyer cannot be sold a loop they cannot read about.

## Fixed here

Facts, not policy:

* `docs/tables.md` gains the `AUTODECLARE` clause in the grammar
  section and a prose passage stating the seven properties above — in
  particular that it is **off unless you ask**, capped by the number
  you write, restricted to columns you declared, and visible as `auto`
  in `IDX.LIST`.
* `docs/rds-workloads.md`'s refusal row keeps its refusal and now says
  where the line is, so the two cannot read as a contradiction.

## Left open (owner's)

The distinction is defensible, but it is a **charter statement** and
the charter has never made it. Two questions only the owner should
answer:

1. **Is bounded auto-declaration inside the boundary, or an exception
   to it?** I have documented it as inside — the line being *decide a
   plan per query* (refused) vs *extend the declaration on request*
   (provided). If the intended line was tighter, `AUTODECLARE` is the
   thing to reconsider, not the docs.
2. **Should `AUTODECLARE_AFTER = 16` be operator-visible?** Today the
   operator sets the budget but not the threshold; 16 is a constant
   with an empirical comment. A workload with a slow-arriving shape
   pays 16 refusals before relief, and cannot tune that.

Neither blocks anything. Both are the kind of thing that should be
decided before the sentence "the boundary is explicit and holds"
appears in sales material.
