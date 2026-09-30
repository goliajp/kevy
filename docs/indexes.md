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
- Up to 64 indexes — a **global budget**, and one worth planning
  against rather than discovering. The rule that makes 64 generous:
  parent-child access paths belong in link keys and sorted sets, which
  cost no index slots. Spend slots only on **global value ranges, text
  and aggregates**. A 58-table schema converted this way needed roughly
  19. Read naively, "58 tables vs 64 indexes" looks nearly blocked; it
  is not, but only if the modelling rule is applied.
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
- `IDX.COUNT <name> RANGE|EQ|WHERE … [FILTER …]…` — count without
  materializing keys. `FILTER` predicates over stored `VALUES` columns
  are applied (the claused count: the total a claused query's pages
  would reach); every clause a count would not apply —
  SORT/DISTINCT/FACET/OFFSET/FIELDS/CURSOR — is refused by name.
- **Non-scalar kinds answer `VERIFY` in their own vocabulary.**
  `KIND agg` answers `rows / bytes / excluded / groups`; `KIND text`
  answers `docs / bytes / postings / tokens`; `KIND ann` answers
  `vectors / bytes / tombstones / links / rebuild_recommended`. None of
  them print `drift` / `missing`, because the audit's question — does
  this entry's row still derive this value — applies to a row-keyed
  entry and their entries are groups, postings and graph nodes. (These
  numbers used to be printed under the scalar labels: a healthy 3-doc
  text index answered `coerce_failures 7, duplicates 7` — its postings
  and token counts wearing an integrity warning's names.)
- **What that means for the aggregate's counters**: the running totals
  are never recomputed against the keyspace at runtime, so their
  agreement with reality rests on every write path maintaining them —
  which `IDX.VERIFY` cannot falsify for this kind. A test does instead:
  `index_write_path_coverage` compares the group's count against the
  live rows after each verb.
- `IDX.VERIFY <name>` — summed stats: entries, bytes,
  coerce_failures, duplicates, plus both directions of the audit:
  `drift` (entries whose row is gone, no longer coerces, or coerces to
  a different value) over `checked` entries, and `missing` (rows under
  the prefix that derive a value and have no entry). Both should be
  zero on a healthy index; `missing` is the direction a walk over the
  index's own entries cannot see. `kevy-cli --kevy doctor` turns that into an
  exit code over every declared table, so "should be zero" can be a
  cron rather than a thing someone remembers to check
  ([table-migration.md](table-migration.md#8-make-verify-part-of-operations-not-part-of-the-migration)).
- `IDX.LIST` — catalog + per-index state/entries/bytes.
- Cursor contract is SCAN-class: rows stable across the whole
  traversal are seen exactly once; concurrent insertions/deletions
  may or may not appear. `"0"` = start / exhausted.

## Uniqueness is a fence, not a lock

A `unique` index **does not block writes** — enforcing global
uniqueness at write time would serialize cross-shard writes. Instead:
duplicates are counted (`duplicates` in
VERIFY/LIST) and visible as multi-hit `EQ` reads.

**The counter is per shard; the read is not.** `duplicates` is
maintained inside each shard's segment, so it sees two rows sharing a
value only when both landed on the same shard — and keys hash across
shards, so the ordinary case is that they did not. Measured: the same
pair of rows reports `duplicates 1` on a single-shard server and
`duplicates 0` on a two-shard one. A global counter would need
cross-shard value counting on the write path, which is the
serialization this kind exists to avoid, so **the multi-hit `EQ` read
is the detection that always works** — `duplicates` is a hint, and a
zero there is not a statement that the value is unique. If you need hard
uniqueness, pin the domain to one shard with a `{hashtag}` prefix in
cluster mode, or check-then-write under `MULTI`/`WATCH`.

**In the embedded API an index cannot be read inside `atomic()` at
all**, so `KIND unique` cannot participate in the check even
optimistically. Use a **claim key** instead: `u:<constraint>:<value>`
holding the owner's id, read with `get` and written with `set` inside
the transaction. The transaction makes the check-and-claim atomic,
which is the guarantee a `unique` index deliberately does not provide.

A consumer implemented 22 uniqueness constraints this way and used
`KIND unique` for none of them; the pattern works, but they arrived at
it by discovering the omission rather than reading it here.

## Embedded

Same engine, typed API: `idx_create / idx_drop / idx_query /
idx_count / idx_stats / idx_list` (values as `IndexValue`, cursors as
`IndexCursor`). No `FIELDS` hydration — you're in-process; read
fields with `hget`. `idx_create` builds synchronously and returns
when the index serves.

## The index budget

**64 indexes, globally.** Not per prefix, not per shard — 64 for the
whole store (`MAX_INDEXES`, `kevy-index/src/catalog.rs`).

Read naively that number blocks any real schema: 58 tables against 64
indexes looks impossible, and a migration can stall on the arithmetic
before discovering that the arithmetic is wrong.

**Indexes are a scarce global budget, and most access paths do not spend
it.** Parent-child navigation belongs in link keys and zsets — a
`SMEMBERS order:1001:items` costs no index slot, and neither does an
ordered zset index you maintain yourself
([cookbook §2](cookbook.md#2-one-to-many-many-to-many)). Spend index
slots only on what link keys cannot express:

- **global value ranges** — "every invoice over 10k", across all rows
- **text search** — `KIND text`
- **aggregates** — `KIND agg`, write-time GROUP BY

A schema that would need 58 indexes read as "one per table" typically
needs under 20 read as "one per global query shape". If you are
approaching 64, the question to ask is which of them are really
parent-child navigation wearing an index costume.

## Global indexes (`PARTITION global`)

An index is local by default: every shard indexes the rows it holds, so
every query goes to every shard and the origin merges their pages. A
**global** index is cut by value into partitions, one per shard. A row
stays on the shard its key hashes to; its entry lives in the partition
its value falls in.

```
IDX.CREATE by_age ON PREFIX user: FIELD age TYPE i64 KIND range PARTITION global SPLIT 30 SPLIT 60
TABLE.DECLARE user PREFIX user: PK id COLUMN id i64 COLUMN age i64 INDEX age range GLOBAL SPLIT AT 30 60
```

- **Split points.** `SPLIT v` (one per point, as the option pairs of
  `IDX.CREATE` require) or `GLOBAL SPLIT AT v…`: at most one fewer than
  the shard count. Without them every shard sorts its rows' values and
  sends them cut into 256 buckets of equal rank per partition, each as
  its largest value and its row count; kevy merges them by count. A split
  is then off by at most one bucket per shard, so the largest partition
  starts within 2/256 (0.8%) of the mean. An index created over no rows has one partition until
  `IDX.REBUILD` samples again. Every entry of one value lives in one
  partition, so a value held by more than its share of rows cannot be
  split. An `ORDERPATH … GLOBAL` samples, or takes `SPLIT AT` points
  written the way `TABLE.DESCRIBE` writes them: `0x` and the path's
  encoded order bytes, since a point there spans several columns.
- **Reads.** An `EQ`, or a `RANGE` inside one partition, reads one shard.
  A page in `(value, key)` order walks the partitions it needs one after
  another and concatenates them — no merge of N pages. `IDX.COUNT` and
  the selection clauses (`SORT`, `DISTINCT`, `FACET`, `OFFSET`) go to the
  partitions the range meets. `IDX.EXPLAIN` names them.
- **Writes.** A write that changes a row's entry sends one message to the
  entry's partition (two when the entry moves partitions), and the
  client's reply waits until the partition has applied it: a read sent
  after the reply sees the write.
- **`FIELDS` come from `VALUES`.** The partition holds the entry, not the
  row, so a global index answers `FIELDS` from the columns it stores. A
  field it does not store is refused by name, and `IDX.ADVISE` suggests
  adding it to `VALUES`.
- **Kinds and limits.** `range` and `unique` only. Not with `COMPOSE` or
  views (both need a row's entries on the row's own shard), not on a
  windowed table, not in an embedded store — each refused by name.
- **Building.** Queries answer `-INDEXBUILDING` until every shard has
  sent the entries of the rows it held; a query never sees a partial
  partition.
- **Operating.** `IDX.LIST` reports `partitioning` for every index, and
  for a global one `partitions`, `max_entries` and `mean_entries` —
  skew shows as the ratio of the last two. `IDX.REBUILD <name>` samples
  again and rebuilds. `IDX.VERIFY` matches every row against the entry
  its partition holds, so `drift` and `missing` are exact, and a global
  unique index counts `duplicates` across the whole keyspace (a local one
  sees duplicates within a shard). A server restarted with fewer shards
  than an index has partitions keeps an even subset of its split points.
- **Memory.** The entries cost what a local index's do, and the row's
  shard keeps nothing per row for a global index: a write names the value
  the row was indexed under, and the value names the partition that holds
  its entry. `IDX.LIST` and `IDX.VERIFY` report the partitions' `bytes`.

## Consistency + cost model

- A write and its index update are atomic within the owning shard
  (single reactor thread / shard lock). Cross-shard queries are
  merged per shard with no global snapshot (SCAN-class, same as
  DBSIZE).
- An **empty catalog costs one untaken branch per write** (a Relaxed
  atomic load). With indexes declared, a write in an indexed domain
  pays one hash-field read and one tree update per matching index. Before
  a write changes a row, the store records its indexed fields, so the
  update knows which entry to drop whatever wrote the row: a command, a
  script's inner call, a transaction, expiry, eviction, a replicated
  frame.
- Memory per index ≈ `rows × ((value_len + handle_len + 3) / fill + 1)`
  bytes. An index is a B+ tree of 1,784-byte leaves; an entry is a
  10-byte slot and whatever of its order key runs past the first eight
  bytes. `value_len` is 8 for `i64` / `f64` and the string's length plus
  2 for `str`; `handle_len` is the key without the index's prefix, half
  that (rounded up) when it is all digits. `fill` is how full the leaves
  are: 1.0 after a build or `IDX.REBUILD`, which pack them, and never
  below two-thirds whatever order rows are written and deleted in: every
  leaf but a segment's first and last keeps two-thirds of its page, less
  two entries. Random writes leave them 0.85–0.9 full. Over 1.25 million
  rows keyed `row:<n>`, an `i64` index measured 15.9 bytes a row packed
  and 16.8 after random writes, a `str` index of ten-byte values 20.1 and
  22.8. `IDX.LIST` and `IDX.VERIFY` report what the leaves hold;
  `bench/idxgate.sh` checks it against the server's measured RSS.
- An index that declares `VALUES` keeps them in the same entry: a
  one-byte tag each, then the value — a number of decimal digits at half
  a byte a digit, anything else as its bytes. Add that to the entry
  above: a short string and a ten-digit number measured about 10 bytes a
  row more. `bytes` includes it.

## Aggregate kind (`KIND agg`) — write-time GROUP BY

```
IDX.CREATE ord_amt ON PREFIX ord: FIELD amount TYPE i64 KIND agg GROUPBY status
IDX.QUERY ord_amt GROUP paid                      → [count, sum, min, max, avg]
IDX.QUERY ord_amt GROUPS BY sum LIMIT 100         → ranked [group, count, sum, min, max]
```

**`GROUPBY` takes one field, and the shape of real `GROUP BY` usually
needs more.** `SUM(amount) GROUP BY month` split by direction, or any
`SUM(CASE WHEN …)`, is expressed by moving the condition **into the
group key**: materialise a composite field at write time
(`ym_dir = "2026-07:in"`), group on it, and split the key in the
application. Conditional aggregates work the same way — the condition
becomes part of what you are grouping by, not part of the aggregate.

That idiom is obvious in hindsight and invisible on first contact:
`KIND agg` looks like it answers `GROUP BY` and then does not answer
the shape people actually write.

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

Memory ≈ `groups × (gkey+64) + distinct_values × 18 + rows ×
(key+10)` (constants calibrated against measured RSS); gated vs real RSS by `bench/agggate.sh` along with GROUP
p99 < 1ms @ 1M×10k groups, GROUPS top-100 < 5ms, and write tax < 10%.

