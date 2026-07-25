# Views (`VIEW.*` / `view_*`)

A view is a **named composition of declared indexes** — an AND/OR/DIFF
tree of index shapes with an ordering index — queryable as one unit,
either evaluated per query (**virtual**) or maintained incrementally
on every write (**materialized**, optionally top-K bounded). Views
are the engine's answer to the "hot list" query — `WHERE state =
'ready' AND pri BETWEEN 0 AND 100 ORDER BY pri DESC LIMIT 10` — as a
declared access path instead of a query-time scan.

```
IDX.CREATE j_pri  ON PREFIX job: FIELD pri  TYPE i64 KIND range
IDX.CREATE j_state ON PREFIX job: FIELD state TYPE str KIND range

VIEW.CREATE ready_jobs
    QUERY ( AND j_pri RANGE 0 100 j_state EQ ready )
    ORDER BY j_pri DESC
    MODE materialized TOPK 100
VIEW.QUERY ready_jobs LIMIT 10
```

## Quick start (server)

Everything a view composes must exist first — leaves and the ORDER
BY index are declared `IDX.*` indexes over the same prefix domain
([indexes.md](indexes.md)):

```console
kevy-cli -p 6004 IDX.CREATE j_pri ON PREFIX job: FIELD pri TYPE i64 KIND range
kevy-cli -p 6004 IDX.CREATE j_state ON PREFIX job: FIELD state TYPE str KIND range
kevy-cli -p 6004 VIEW.CREATE ready_jobs QUERY ( AND j_pri RANGE 0 100 j_state EQ ready ) ORDER BY j_pri DESC MODE materialized TOPK 100
kevy-cli -p 6004 HSET job:1 pri 10 state ready
kevy-cli -p 6004 HSET job:2 pri 99 state blocked
kevy-cli -p 6004 VIEW.QUERY ready_jobs LIMIT 10
```

The reply pages `key, order-value` pairs in view order with a resume
`CURSOR`. The tree grammar (one line, parenthesized):

```
tree     = '(' AND|OR|DIFF sub sub ')' | leaf
leaf     = <index> RANGE <min> <max> | <index> EQ <value>
```

Supporting verbs: `VIEW.LIST` (catalog + mode + shape),
`VIEW.EXPLAIN name` (the tree with per-leaf cardinalities),
`VIEW.VERIFY name` (members / bytes / order-exclusions — **bytes and
order-exclusions are materialized-only**; a virtual view stores nothing and
reports both as 0, and verifying one costs a full fresh `eval_tree`, which is
the one place a virtual view is the expensive one),
`VIEW.REBUILD name` (force a materialized rebuild — answer-
preserving), `VIEW.DROP name`.

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
  fresh; zero write cost. The failure mode is a highly selective
  tree under a large ORDER domain (many candidates probed per
  emitted row) — that shape wants materialization.
- **Materialized** — an ordered member set per shard, updated in the
  same write hook that maintains indexes (one probe per referenced
  index per write, shared across all views). `TOPK k` bounds it to
  `k + k/4`, evicting from the view's worst end; a non-member worse
  than the current worst is rejected with a single comparison — the
  steady-state write tax measured by
  [`bench/viewgate.sh`](../bench/viewgate.sh) is ~2% for 3 indexes +
  4 top-K views (the gate clamps it < 15%). Shrinking below `k`
  schedules a local per-shard rebuild (next tick). Unbounded
  materialized views pay O(log members) per affected write.
  - Right after `VIEW.CREATE`, expect a brief settling window (the
    first write burst over a fresh top-K set runs slower while the
    eviction threshold stabilizes).

Choosing: a dashboard top-100 over a high-write domain wants
`MODE materialized TOPK 100` (bounded memory, O(1) rejection of
non-contenders); an occasionally-paged report over a moderate domain
wants `MODE virtual` (zero write tax, always fresh). `VIEW.EXPLAIN`
shows per-leaf cardinalities — the selectivity data for the call.

### DIFF: the declared anti-join

`DIFF` is left-minus-right and NOT commutative — the one tree node
whose children the engine never re-orders. The canonical use is
"eligible but not excluded":

```
VIEW.CREATE assignable
    QUERY ( DIFF j_pri RANGE 0 100 j_state EQ quarantined )
    ORDER BY j_pri DESC
    MODE virtual
```

— every job in the working priority band, minus the quarantined
ones.

In SQL terms this is the `WHERE … AND NOT EXISTS (…)` shape — as a
declared access path instead of a per-query subquery
([rds-workloads.md](rds-workloads.md) maps the rest of that
vocabulary).

## Consistency

Same envelope as indexes ([indexes.md](indexes.md)): per-shard
atomic with the triggering write, cross-shard merged without a
global snapshot (SCAN-class).

- Queries answer `-INDEXBUILDING` while any referenced index is
  still backfilling (a partial index would silently misreport
  membership) — same retry discipline as index queries.
- `VIEW.REBUILD` is answer-preserving (asserted in the e2e suite);
  `VIEW.VERIFY` makes drift falsifiable (members / bytes /
  order-exclusions).
- The view catalog persists in a data-dir sidecar; materialized
  CONTENT is derived state — rebuilt after restart, never
  snapshotted.

## Embedded

Typed API behind the `index` cargo feature (on by default) — the
tree is passed as a value, no text grammar in-process, and every
call returns `KevyResult` (4.0's single error currency):

```rust
use kevy_embedded::{
    Config, IndexKind, IndexValType, IndexValue, Store, ViewLeaf,
    ViewMode, ViewTree,
};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(Config::default())?;
    store.idx_create(b"j_pri", b"job:", b"pri", IndexValType::I64,
                     IndexKind::Range)?;
    store.idx_create(b"j_state", b"job:", b"state", IndexValType::Str,
                     IndexKind::Range)?;

    let tree = ViewTree::And(
        Box::new(ViewTree::Leaf(ViewLeaf {
            index: b"j_pri".to_vec(),
            min: IndexValue::I64(0),
            max: IndexValue::I64(100),
        })),
        Box::new(ViewTree::Leaf(ViewLeaf {
            index: b"j_state".to_vec(),
            min: IndexValue::Str(b"ready".to_vec()),
            max: IndexValue::Str(b"ready".to_vec()),   // EQ = same min/max
        })),
    );
    store.view_create(b"ready_jobs", tree, b"j_pri", /*desc*/ true,
                      ViewMode::Materialized { top_k: 100 })?;

    store.hset(b"job:1", &[
        (b"pri".as_slice(), b"10".as_slice()),
        (b"state".as_slice(), b"ready".as_slice()),
    ])?;

    // One page: (Vec<(key, order_value)>, Option<resume_cursor>).
    let (rows, _next) = store.view_query(b"ready_jobs", None, 10)?;
    for (key, val) in &rows {
        println!("{} {val:?}", String::from_utf8_lossy(key));
    }
    Ok(())
}
```

- `view_create(name, tree, order_by, desc, mode)` builds
  synchronously; every referenced index (leaves + ORDER BY) must
  already be declared (`KevyError::InvalidInput` otherwise).
- `view_query(name, after, limit)` pages `(key, order_value)` rows;
  the returned cursor resumes exclusively (DESC views page from the
  large end). `view_count` / `view_list` / `view_drop` complete the
  face.
- No `VIA`/`FIELDS` — in-process callers dereference and read fields
  directly with `hget`.

## Performance

Measured envelope — the clamps live in
[`bench/viewgate.sh`](../bench/viewgate.sh), run against a real
server at 1M rows:

- Virtual `VIEW.QUERY` p99 < 3ms (two-component tree).
- Materialized `VIEW.QUERY` p99 < 2ms (the index-read line — a
  materialized page IS an ordered-set read).
- Write tax with 3 indexes + 4 materialized top-K views < 15% vs the
  same workload with indexes only (measured steady-state ~2%).
- Memory per member within ±20% of the formula below.

Memory per member ≈ `order_value_width + key_len + 48` bytes,
reported live by `VIEW.VERIFY`.

## See also

- [indexes.md](indexes.md) — the indexes views compose (declaration,
  backfill, the consistency envelope views inherit).
- [rds-workloads.md](rds-workloads.md) — where views sit in the
  SQL-to-kevy mapping.
- [verb-reference.md](verb-reference.md) — generated grammar for
  every `VIEW.*` form.
- [cookbook.md](cookbook.md) — view recipes in context.
