# Secondary indexes (`IDX.*` / `idx_*`)

kevy can maintain declarative secondary indexes over a **prefix
domain** of the keyspace: every hash key under a prefix is a "row",
one declared hash field is the indexed value. Indexes are maintained
**synchronously with every write** (derived-by-construction — the
index can never drift from the data, and `IDX.VERIFY` makes that
falsifiable), and queried with cursor pagination, two-index
composition, and optional field hydration.

```
IDX.CREATE idx_age ON PREFIX user: FIELD age TYPE i64 KIND range
HSET user:42 age 31 name "……"
IDX.QUERY idx_age RANGE 18 30 LIMIT 100 FIELDS name
```

## Declaring

`IDX.CREATE <name> ON PREFIX <p> FIELD <f> TYPE i64|f64|str KIND
range|unique [MAXMEM <bytes>]`

- **TYPE** is a scalar coercion: a row whose field is missing or fails
  to parse is **excluded** (counted per index — `IDX.VERIFY` /
  `IDX.LIST` report `coerce_failures`; this is the declarative fence,
  not a runtime error).
- **KIND range** serves `RANGE min max` scans; **unique** serves the
  same plus the duplicate fence (below).
- **MAXMEM** caps the index's memory: a build that crosses the budget
  fails declaratively (`-INDEXOVERBUDGET` on queries) instead of
  growing unbounded.
- Up to 64 indexes. The catalog persists in a data-dir sidecar;
  the index CONTENT is derived state — it is never snapshotted or
  AOF-logged, and rebuilds in the background after a restart
  (`-INDEXBUILDING` until ready; data availability never waits).

## Querying

- `IDX.QUERY <name> RANGE <min> <max> | EQ <v> [LIMIT n] [CURSOR c]
  [FIELDS f…]` → `[next-cursor, rows]`. Rows are `(value, key)`
  ordered across all shards; `FIELDS` hydrates the named hash fields
  on each row's owning shard (no second round-trip) and switches the
  rows to nested `[key, value, fname, fval…]` form.
- `IDX.QUERY COMPOSE AND|OR <n1> <spec1> <n2> <spec2> …` — two-index
  composition, **key-ordered** (the two value domains differ), same
  LIMIT/CURSOR/FIELDS tail. AND/OR run per shard (a key lives on
  exactly one shard, so per-shard set algebra composes globally).
- `IDX.COUNT <name> RANGE|EQ …` — count without materializing keys.
- `IDX.VERIFY <name>` — summed stats: entries, bytes,
  coerce_failures, duplicates.
- `IDX.LIST` — catalog + per-index state/entries/bytes.
- Cursor contract is SCAN-class: rows stable across the whole
  traversal are seen exactly once; concurrent insertions/deletions
  may or may not appear. `"0"` = start / exhausted.

## Uniqueness is a fence, not a lock

A `unique` index **does not block writes** — enforcing global
uniqueness at write time would serialize cross-shard writes (see the
design RFC). Instead: duplicates are counted (`duplicates` in
VERIFY/LIST) and visible as multi-hit `EQ` reads. If you need hard
uniqueness, pin the domain to one shard with a `{hashtag}` prefix in
cluster mode, or check-then-write under `MULTI`/`WATCH`.

## Embedded

Same engine, typed API: `idx_create / idx_drop / idx_query /
idx_count / idx_stats / idx_list` (values as `IndexValue`, cursors as
`IndexCursor`). No `FIELDS` hydration — you're in-process; read
fields with `hget`. `idx_create` builds synchronously and returns
when the index serves.

## Consistency + cost model

- A write and its index update are atomic within the owning shard
  (single reactor thread / shard lock). Cross-shard queries are
  merged per shard with no global snapshot (SCAN-class, same as
  DBSIZE).
- An **empty catalog costs one untaken branch per write** (a Relaxed
  atomic load). With indexes declared, a write in an indexed domain
  pays one hash-field read + one B-tree update per matching index.
- Memory per index ≈ `rows × (value_width + avg_key_len + 48)` bytes
  (the constant is per-entry structure overhead). `IDX.LIST` reports
  measured bytes; `bench/idxgate.sh` gates the formula.

## Aggregate kind (`KIND agg`) — write-time GROUP BY

```
IDX.CREATE ord_amt ON PREFIX ord: FIELD amount TYPE i64 KIND agg GROUPBY status
IDX.QUERY ord_amt GROUP paid                      → [count, sum, min, max, avg]
IDX.QUERY ord_amt GROUPS BY sum LIMIT 100         → ranked [group, count, sum, min, max]
```

The engine answer to `SELECT g, COUNT(*), SUM(v) … GROUP BY g`:
aggregates are maintained IN THE WRITE PATH (a declared access path —
never a query-time row scan). min/max stay exact under deletion via a
per-group value multiset; sums accumulate in f64 (documented
precision bound); a row whose value fails coercion or whose group
field is missing is excluded and counted (VERIFY). Cross-shard
merging is exact: counts/sums add, extremes take. `GROUPS` ranks by
count/sum/max descending or min ascending, LIMIT ≤ 1000.

No HAVING, no aggregate expressions, no approximate sketches — the
query-language slope. Filter GROUPS results in the app.

Embedded: `idx_create_agg(name, prefix, field, ty, group_by)` /
`idx_group(name, g)` / `idx_groups(name, by, limit)`.

Memory ≈ `groups × (gkey+64) + distinct_values × 40 + rows ×
(key+24)`; gated vs real RSS by `bench/agggate.sh` along with GROUP
p99 < 1ms @ 1M×10k groups, GROUPS top-100 < 5ms, and write tax < 10%.

