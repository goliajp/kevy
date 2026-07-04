# Views (`VIEW.*` / `view_*`)

A view is a **named composition of declared indexes** — an AND/OR/DIFF
tree of index shapes with an ordering index — queryable as one unit,
either evaluated per query (**virtual**) or maintained incrementally
on every write (**materialized**, optionally top-K bounded).

```
IDX.CREATE j_pri  ON PREFIX job: FIELD pri  TYPE i64 KIND range
IDX.CREATE j_state ON PREFIX job: FIELD state TYPE str KIND range

VIEW.CREATE ready_jobs
    QUERY ( AND j_pri RANGE 0 100 j_state EQ ready )
    ORDER BY j_pri DESC
    MODE materialized TOPK 100
VIEW.QUERY ready_jobs LIMIT 10
```

## The three structural rules

1. **Components are named indexes.** Leaves carry a shape (`RANGE min
   max` | `EQ v`, coerced to the referenced index's type at CREATE);
   the view layer holds no predicates of its own. Trees are depth ≤ 3,
   ≤ 4 leaves; `AND`/`OR` may be re-ordered by the engine, `DIFF` is
   fixed left-minus-right.
2. **A view stores membership + order only** — never field values.
   `ORDER BY <index>` supplies the sort key; rows absent from the
   order index are excluded (counted, visible in `VIEW.VERIFY`).
3. **Hydration is dereference, not query.** `VIA <template>` (e.g.
   `user:{key.1}`; `{key}` = member key, `{key.N}` = its N-th
   `:`-segment) maps each member to a target key; `VIEW.QUERY …
   FIELDS f…` reads those fields on the targets' owning shards in a
   second internal fan-out. Missing target = nil row fields. Targets
   take no predicates.

## Modes and cost model

- **Virtual** — evaluated at query time by streaming the ORDER index
  in order and probing tree membership per candidate: a LIMIT-100
  page costs O(limit / selectivity) probes, not O(members). Always
  fresh; zero write cost.
- **Materialized** — an ordered member set per shard, updated in the
  same write hook that maintains indexes (one probe per referenced
  index per write, shared across all views). `TOPK k` bounds it to
  `k + k/4`, evicting from the view's worst end; a non-member worse
  than the current worst is rejected with a single comparison — the
  steady-state write tax measured by `bench/viewgate.sh` is ~2% for
  3 indexes + 4 top-K views. Shrinking below `k` schedules a local
  per-shard rebuild (next tick). Unbounded materialized views pay
  O(log members) per affected write.
  - Right after `VIEW.CREATE`, expect a brief settling window (the
    first write burst over a fresh top-K set runs slower while the
    eviction threshold stabilizes).
- Queries answer `-INDEXBUILDING` while any referenced index is still
  backfilling (a partial index would silently misreport membership).
- `VIEW.REBUILD` is answer-preserving (asserted in the e2e suite);
  `VIEW.VERIFY` reports members / bytes / order-exclusions;
  `VIEW.EXPLAIN` renders the tree with per-leaf cardinalities.

## Consistency

Same envelope as indexes: per-shard atomic with the triggering write,
cross-shard merged without a global snapshot (SCAN-class). The view
catalog persists in a sidecar; materialized CONTENT is derived state —
rebuilt after restart, never snapshotted.

## Embedded

Typed API: `view_create(name, Tree, order_by, desc, mode)` /
`view_query` / `view_list` / `view_count` / `view_drop`. No
`VIA`/`FIELDS` — in-process callers dereference and read fields
directly. Memory per member ≈ `order_value_width + key_len + 48`
bytes (gated ±20% by viewgate).
