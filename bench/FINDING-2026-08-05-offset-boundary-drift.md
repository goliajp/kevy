# OFFSET: refused in the charter, shipped in the engine, used in production

First of the three blockers the v5 RDS ledger names
(`.claude/plans/2026-08-05-v5-status.md` §二.3 — "the declared boundary
has drifted"). Taken one at a time; this is the first.

## The inconsistency

`docs/rds-workloads.md:435` — the charter's refusal table, described in
its own header as *"each is a charter decision … not a roadmap gap"*:

| Refused | Because | Use instead |
|---|---|---|
| `OFFSET` pagination | O(N) skip is an anti-pattern | cursors |

But:

* the engine **parses and executes it** —
  `cmd_index_query/args_scalar.rs:45,59`, and `OFFSET` is one of the
  clauses that routes a query to the claused reduce;
* `docs/tables.md:225,230` **documents it as a supported clause**, in an
  example, on the same TABLE surface;
* a real consumer **uses it in two places** (R4a inventory finding 7 —
  `ScalarQueryOpts.offset` and a manual `drain(offset)` after a merge).

So one document refuses what another documents and the engine ships.
A reader cannot find out which is true without reading the source.

## What it actually costs (measured, not asserted)

The refusal's stated reason is a cost claim, so it is worth a number
rather than a taste. 30 000 rows inside the queried range,
`LIMIT 20`, p50 of 30 calls, one box:

| OFFSET | 1 shard | 8 shards |
|---|---:|---:|
| 0 | 219 µs | 526 µs |
| 100 | 369 µs | 1.30 ms |
| 1 000 | 1.53 ms | 6.90 ms |
| 10 000 | **11.5 ms** | **20.1 ms** |
| 25 000 | 11.6 ms | 20.9 ms (saturated — the range holds 30 k) |

Two facts, both load-bearing:

1. **It is linear in the offset.** 10× the offset is ~10× the latency
   (1 shard: 1.53 ms → 11.5 ms). The charter's "O(N) skip" is
   **correct** — this is not a stale worry.
2. **Fan-out multiplies it, so more cores make it worse.** At
   `OFFSET 1000`, eight shards cost 4.5× one shard. The mechanism is in
   the code and is not incidental: each shard fetches `limit + offset`
   because *"a shard cannot know which of its hits survive"* the global
   merge (`query_claused.rs:86-89`), so the origin materialises
   `(limit + offset) × width` hits to return `limit`. Adding hardware
   makes this query slower, which is the opposite of what every other
   axis in this engine does.

For scale: `20 ms to return 20 rows` on a 30 k-row range. R4a's
inventory records real production incidents in the 3.6–19 s band, all
read-side. A deep OFFSET on a real table is the same shape.

## Where this leaves the boundary

The refusal's *reasoning* is measurably right. The *statement* is
factually wrong — the engine does not refuse. Those are separable, and
only the second is mine to fix:

* **Fixed here (a fact, not a policy):** the refusal table no longer
  claims `OFFSET` is refused. It states what is true — provided on the
  claused surface, linear in the offset, worse with width — with the
  numbers above and the cursor alternative kept as the recommendation.
  `docs/tables.md` gains the same cost note where it shows the clause,
  so the two faces stop disagreeing.
* **NOT decided here (policy, owner's):** what the boundary *should*
  be. Three shapes, with consequences:
  1. **Leave it provided + documented cost.** Nothing breaks. The
     charter's "no accidental O(n)" principle keeps one exception, and
     the exception is now signposted rather than silent.
  2. **Named refusal above a threshold** (`OFFSET > N` → refuse by
     name, point at cursors / `IDX.ADVISE`). This is the genre answer —
     kevy refuses by name everywhere else, and R4a finding 8 credits it
     for being *more honest than SQL* by surfacing `duplicates`. But it
     can break a shipped consumer at runtime, and I do not know what
     offsets that consumer actually reaches; that has to be measured
     against them before any threshold is picked.
  3. **Deprecate toward cursors** with a release cycle of warning.
     Slowest, cleanest, needs a deprecation channel this engine does
     not currently have.

**The recommendation is 2, gated on first measuring the real offsets in
use.** Not 1, because "one signposted exception" is how a boundary
starts leaking — and this whole finding exists because a boundary
leaked. Not 3 yet, because there is no deprecation surface to use.

## The general lesson for the other two blockers

This one was found by the inventory, not by a gate: **nothing in CI
checks the refusal table against the engine.** Every row in it is a
claim about behaviour, and claims about behaviour are testable. The
other thirteen rows were checked by hand for this finding (below) — but
by hand is exactly how it drifted in the first place.

### The other thirteen, checked

| Row | Verdict |
|---|---|
| SQL parser / query DSL | **consistent** — `kevy-sql` is out-of-engine and declaration-time by its own contract; ad-hoc runtime SQL is still an unknown command to the engine |
| query planner / auto index selection | **needs reconciling — see below** |
| JOINs | consistent — `VIA` is listed in the table's own "use instead" column |
| `WHERE` without an index | consistent — the prefix law refuses by name |
| constraint DSL / triggers | consistent — Lua is the sanctioned unit |
| DECIMAL | consistent — no decimal type exists |
| JSON-path queries | consistent — none |
| `HAVING` / aggregate expressions | consistent — `kevy-sql` refuses by name with line/column |
| multi-database `SELECT n` | consistent — only `SELECT 0` |
| AUTH / TLS | consistent — permanently out |
| cross-DC active-active, CRDTs | consistent |
| dynamic membership / auto-replace / resharding | consistent in spirit — reshard and MOVE-SCOPE are operator-driven, which is what the row's "topology is operator-declared" means |
| HTTP/REST API | consistent in substance — the only HTTP listener serves `GET /metrics` and 404s everything else, so RESP and MCP remain the access planes; the row's flat "HTTP" wording predates that endpoint and is worth a parenthesis |

**The second row is the other real drift** and is written up separately:
`AUTODECLARE` has the engine deciding *which access paths to declare*
from observed refusals, while the row says the engine "must execute
declared paths, not decide". Whether that is a contradiction or a
distinction (decide once at declaration time vs per query) is a
statement the charter has to make in its own voice — it currently does
not.
