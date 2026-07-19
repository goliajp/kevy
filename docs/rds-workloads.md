# RDS workloads on kevy

You run an operational workload on MySQL or PostgreSQL and are
considering moving it — wholly or partially — onto kevy. This page is
the **reference matrix**: every SQL-surface construct, what carries it
in kevy, the exact verbs, and the semantic deltas. Where kevy refuses a
construct, the refusal is stated plainly along with the covered
alternative. The companion [cookbook](cookbook.md) turns these rows
into runnable recipes; [designing-on-kevy](designing-on-kevy.md) is
the one-page map of the engine itself.

## The division of labor

An RDS lets you write data first and decide the access paths later —
the query planner turns any `WHERE` clause into *some* plan at query
time. kevy inverts that: **you declare the access paths at write time**
(indexes, aggregates, views), the engine maintains them synchronously
with every write (derived-by-construction, zero drift), and serving is
a microsecond-scale lookup, never a plan search. The trade is
explicit:

- What you give up: ad-hoc queries over undeclared paths. A `WHERE`
  with no matching index is not slow on kevy — it is *absent by
  design* (model the access path or don't ship the query).
- What you get: predictable serving latency (hydrated page p99 < 1ms,
  write fan-out through index+view hooks p99 < 200µs on one server
  carrying the full stack — the gated lines in
  [designing-on-kevy](designing-on-kevy.md)), and raw Redis-parity
  throughput far above what a B-tree-on-disk engine serves
  (see [Sizing](#sizing-and-operational-deltas)).

The boundary is a charter, not a backlog: no query language, no
planner, no joins, no server-side validation DSL, no triggers
("Law 3", [designing-on-kevy](designing-on-kevy.md)). Everything below
maps SQL constructs onto that fixed surface.

## Tables, rows, columns

| RDS | kevy |
|---|---|
| table | key prefix (`user:`) |
| row | hash under the prefix (`user:42`) |
| column | hash field |
| PRIMARY KEY | the key itself |
| NULL | absent field (never a sentinel string) |

```
HSET user:42 name ada email ada@example.com age 36
HGETALL user:42                # SELECT * WHERE id = 42
HGET user:42 email             # SELECT email WHERE id = 42
HMGET user:42 name age         # SELECT name, age WHERE id = 42
```

`HGET` of a missing field answers nil — that IS the NULL semantics,
and index specs treat a missing field as "row excluded" (counted,
visible in `IDX.VERIFY`).

### Type system

kevy stores bytes; types are declared **where they matter — at index
creation** (`TYPE i64|f64|str|vector`). A row whose field fails the
declared coercion is excluded from that index and counted
(`coerce_failures` in `IDX.VERIFY` / `IDX.LIST`) — a declarative
fence, never a write error.

| RDS type | kevy form | notes |
|---|---|---|
| INT / BIGINT / SERIAL | field bytes, index `TYPE i64` | full i64 range |
| DECIMAL / NUMERIC | **integer minor units** as `i64` (cents, micros) | kevy has no decimal type; `f64` is binary floating point — never store money in it. Sums in `KIND agg` accumulate in f64 (documented precision bound), so pick units that keep totals well under 2^53 |
| FLOAT / DOUBLE | field bytes, index `TYPE f64` | IEEE 754 semantics |
| VARCHAR / CHAR / TEXT | field bytes, index `TYPE str` | binary-safe; lexicographic byte order |
| BLOB / BYTEA | field bytes (no index) or a dedicated string key | values are arbitrary bytes everywhere |
| DATETIME / TIMESTAMP | Unix epoch (secs or millis) as `i64` | range index gives you `BETWEEN`; kevy has no date arithmetic — the app does calendar math |
| BOOL | `0` / `1` as `i64` | indexable, composable (`EQ 0`) |
| JSON / JSONB | flattened hash fields (`profile.city`) | per-field reads, TTLs (`HEXPIRE`), indexability; JSON-path queries are permanently out — cookbook recipe 9 |
| embedding (pgvector) | `dim × 4` bytes f32-LE, index `TYPE vector` | [vector-search](vector-search.md) |

## PRIMARY KEY, UNIQUE, AUTO_INCREMENT

**Primary key** — the key itself. Point lookups (`WHERE pk = ?`) are
`GET`/`HGETALL`/`HMGET`: one shard, one hash probe, no index needed.

**AUTO_INCREMENT / sequences** — `INCR seq:order` mints one id;
`INCRBY seq:order 100` allocates a block to hand out from app memory
(the high-throughput form). Gaps on crash are the same contract
PostgreSQL sequences give you (cookbook recipe 3).

**UNIQUE** — `KIND unique` is a **fence, not a lock**: it does not
block writes (blocking would serialize cross-shard writes); duplicates
are counted (`duplicates` in `IDX.VERIFY`) and visible as multi-hit
`EQ` reads. For a hard uniqueness gate use one of:

```
SET uniq:email:ada@example.com 42 NX          # atomic claim, NX = the gate
WATCH + MULTI/EXEC check-then-write           # CAS loop (cookbook recipe 4)
{hashtag} prefix + single-shard domain        # cluster mode
```

## SELECT

| SQL | kevy | verbs |
|---|---|---|
| `WHERE pk = ?` | direct key read | `GET` / `HGETALL` / `HMGET` |
| `WHERE col = ?` | equality on a declared index | `IDX.QUERY idx EQ v [FIELDS f…]` |
| `WHERE col BETWEEN a AND b` | range scan on a declared index | `IDX.QUERY idx RANGE a b` |
| `WHERE col > a` | half-open range | `RANGE a <max-sentinel>` |
| `WHERE a = ? AND b BETWEEN …` | two-index composition | `IDX.QUERY COMPOSE AND idxA EQ v idxB RANGE a b` |
| `WHERE col IN (v1, v2)` | two-leg OR, or N app-side EQ queries | `IDX.QUERY COMPOSE OR idx EQ v1 idx EQ v2` |
| `LIKE 'abc%'` (prefix) | str range index, prefix bounds | `IDX.QUERY idx RANGE 'abc' 'abc\xff'` |
| `LIKE '%abc%'` (infix) | **not supported as such** | full-text `MATCH` if token-shaped; else redesign the field |
| full-text search | `KIND text`, BM25 | `IDX.QUERY idx MATCH "…"` |
| `SELECT col1, col2` (projection) | field hydration | `… FIELDS col1 col2` |
| `COUNT(*) WHERE …` | count without materializing | `IDX.COUNT idx RANGE a b` |
| `EXPLAIN` | parse + plan line, zero execution | `IDX.EXPLAIN idx RANGE a b` |

Semantics and limits, honestly:

- **One field per index.** An index covers one declared field of one
  prefix. Multi-column predicates are `COMPOSE AND|OR` of exactly
  **two** indexes (key-ordered — the two value domains differ, so key
  order is the only shared order), or a [view](#view) for a named,
  reusable composition of up to 4 leaves. Wider ad-hoc conjunctions
  are the query-planner slope — refused. Filter residual predicates
  app-side after hydration.
- **`RANGE`/`EQ`/`COMPOSE`**: default `LIMIT 100`, max `10000`, cursor
  pagination (`[next-cursor, rows]`, `"0"` = start/exhausted).
  Cursor contract is SCAN-class: rows stable across the traversal are
  seen exactly once; concurrent writes may or may not appear. No
  global snapshot — same envelope as `SCAN`/`DBSIZE`.
- **Prefix `LIKE`**: a `TYPE str KIND range` index is ordered by
  lexicographic byte order, so `LIKE 'abc%'` is
  `RANGE 'abc' 'abc<0xff>'` — an index scan, not a keyspace walk.
  (Key-name prefix matching via `SCAN 0 MATCH 'user:*'` also exists
  but walks the whole keyspace incrementally — use it for ops chores,
  never as a serving path.)
- **`MATCH`** is OR-semantics over query tokens, BM25-ranked, CJK
  bigrams built in; `LIMIT` caps at 1000, no cursor; no phrase
  queries, no boolean syntax, no highlighting
  ([text-search](text-search.md)).
- **`FIELDS`** hydrates named hash fields on each row's owning shard
  in the same call — the one-hop replacement for the "index scan +
  primary lookup" double hop. This is also the JOIN-shaped hydration
  primitive (below).
- **`IDX.EXPLAIN` is diagnostic only.** It reports kind, state,
  `est_rows`, and the plan line for the query you already wrote —
  there is no optimizer choosing among plans.

## ORDER BY / LIMIT / OFFSET

- A `range` index **is** the order: `IDX.QUERY … RANGE` returns rows
  ascending by indexed value. `ORDER BY col ASC LIMIT n` = declare a
  range index on `col`, query with `LIMIT n`.
- **Descending order** on the bare index surface: not an `IDX.QUERY`
  option. Either declare a view (`VIEW.CREATE … ORDER BY idx DESC`) or
  store a negated/complemented sort field at write time.
- **`ORDER BY a, b`** (composite): encode the composite into one
  indexed field at write time — `a * 1_000_000 + b` for bounded
  integers, zero-padded strings for lexicographic composites
  (cookbook recipe 8).
- **`OFFSET` does not exist.** There is deliberately no "skip N rows"
  — it is O(N) waste in any engine and an anti-pattern at depth.
  Paginate with the returned cursor (constant cost per page,
  position-stable under the SCAN-class contract). If your product
  needs "jump to page 47", precompute page anchors or rethink the UX;
  kevy will not hide the cost.

## GROUP BY and aggregates

`KIND agg` maintains aggregates **in the write path** — the inversion
of `GROUP BY`: an RDS scans rows at query time, kevy folds each write
into its group as it lands, and reading a group is O(1).

```
IDX.CREATE ord_amt ON PREFIX ord: FIELD amount TYPE i64 KIND agg GROUPBY status
IDX.QUERY ord_amt GROUP paid                  → [count, sum, min, max, avg]
IDX.QUERY ord_amt GROUPS BY sum LIMIT 100     → ranked [group, count, sum, min, max]
```

- `GROUP g` = `SELECT COUNT(*), SUM(v), MIN(v), MAX(v), AVG(v) WHERE
  group = g`; `GROUPS BY count|sum|min|max` = the ranked
  `GROUP BY … ORDER BY agg LIMIT n` (LIMIT ≤ 1000).
- min/max stay exact under deletion (per-group value multiset); sums
  accumulate in f64 — mind the precision bound for money (use minor
  units sized so totals stay well under 2^53).
- **No `HAVING`, no aggregate expressions, no `GROUP BY` over
  arbitrary predicates** — filter the (bounded) `GROUPS` result in
  the app. One group-by field per index; declare several agg indexes
  for several groupings.

## JOIN

**kevy does not join.** No verb joins two prefixes server-side, and
none ever will (Law 3). The three replacements, in order of
preference:

1. **Denormalize at write time.** The serving-model answer: if the
   page shows `order.total` next to `user.name`, write `user_name`
   into the order row when it's created. Storage is cheap; the write
   hook keeps any indexes on the copy fresh. Update fan-out on the
   parent (rare for display fields) is a CDC consumer.
2. **Application-side hydration (the two-hop).** Foreign key lives in
   the row; an index makes the reverse direction one hop:
   `IDX.QUERY order_user EQ 42 FIELDS total status` is
   `SELECT total, status FROM orders WHERE user_id = 42` — the index
   finds the keys and hydrates fields in the same call (cookbook
   recipe 2). The forward direction is `HMGET` per parent — batch the
   ids and pipeline.
3. **A view with `VIA` hydration.** `VIEW.QUERY v FIELDS name VIA
   user:{key.1}` dereferences each member key to a *target* key via a
   template and reads fields there — a declared, reusable
   member→parent dereference (one internal fan-out, no client round
   trips). Dereference, not query: targets take no predicates.

What you lose vs SQL: arbitrary N-way joins, join-time filtering on
the far table, planner-chosen join order. What you gain: no join ever
shows up in a latency histogram.

## VIEW

`VIEW.*` is a named AND/OR/DIFF composition over declared indexes with
an ordering index — closer to a *materialized* view with an index than
to a SQL view:

| SQL view property | kevy view |
|---|---|
| arbitrary SELECT body | **no** — tree of index shapes only (depth ≤ 3, ≤ 4 leaves, shapes fixed at CREATE) |
| always-fresh (virtual) | `MODE virtual` — evaluated per query, zero write cost |
| materialized + REFRESH | `MODE materialized` — incrementally maintained in the write hook, **never stale**, no refresh job; optional `TOPK k` bound |
| ORDER BY in the view | `ORDER BY <index> [DESC]` — a declared range index supplies the order |
| view over a view | no — one level, over indexes |

```
VIEW.CREATE live_adults QUERY '( AND user_live EQ 0 user_age RANGE 18 200 )'
    ORDER BY user_age MODE materialized TOPK 100
VIEW.QUERY live_adults LIMIT 10 [FIELDS name] [VIA …]
```

The killer app is the **hot list**: "top 100 ready jobs by priority"
as a `TOPK` materialized view costs ~2% write tax and answers in
microseconds, permanently fresh — the thing an RDS needs a covering
index + a disciplined query + luck to approximate
([views](views.md)).

## Transactions

kevy's transaction story is Redis's (Law 1), which maps to SQL like
this:

| SQL | kevy | delta |
|---|---|---|
| `BEGIN … COMMIT` (batch) | `MULTI` … `EXEC` | commands queue and apply as one unit; **no interactive reads inside** — reads you branch on happen before `MULTI` |
| `SELECT … FOR UPDATE` | `WATCH key` + `MULTI`/`EXEC` | optimistic CAS, not a lock: `EXEC` answers nil if a watched key changed — re-read and retry (cookbook recipe 4) |
| `ROLLBACK` | none | a queue-time error aborts the whole batch (`-EXECABORT`, nothing ran); a runtime error inside `EXEC` does **not** undo the other commands — Redis semantics |
| stored procedure / one atomic RMW | `EVAL` (Lua) | the whole script is one atomic unit on `KEYS[1]`'s shard; read-decide-write with real branching ([lua](lua.md)) |
| serializable single-entity txn | embedded `store.atomic(key, …)` | shard-locked closure: reads see your own writes, commit is atomic + one fsync (cookbook recipe 5) |
| cross-entity serializable | embedded `atomic_all_shards` | deterministic lock order; the hammer — use sparingly |

Isolation, honestly: kevy has no MVCC and no isolation-level knob.
Within one shard, every command (and every `EVAL`, `MULTI` batch, or
embedded atomic block) is serialized — you get *serializable* behavior
for anything that fits one shard / one script. Across shards there is
no global snapshot: multi-key reads (`MGET`, `IDX.QUERY` merges) are
per-shard-atomic, SCAN-class overall. There is no read that observes a
torn single command, and no dirty read of an uncommitted `MULTI`
batch; there IS no repeatable-read across two separate commands —
use `WATCH` (CAS) or Lua (one unit) where that matters.

Durability of a "commit": see [persistence](persistence.md) — with
`appendfsync always` an acknowledged write is on disk before the
reply; the default `everysec` windows up to 1s (the Redis trade).
`Store::fsync_aof()` (embedded) is the per-transaction
`synchronous_commit` escape hatch.

## Constraints and triggers

| SQL | kevy answer |
|---|---|
| `NOT NULL` / type checks | the **coercion fence**: rows failing an index's declared TYPE are excluded and counted (`coerce_failures`); the app watches `IDX.VERIFY` — declarative visibility, not write rejection |
| `CHECK (expr)` | evaluate the invariant inside the atomic unit: Lua script (server) or `atomic` block (embedded) reads, decides, writes — the engine guarantees decision + commit are one unit (cookbook recipe 5) |
| `UNIQUE` | fence + counted duplicates, or a hard `SET … NX` gate (above) |
| `FOREIGN KEY` (existence) | not enforced; write parent-then-child inside one atomic unit if you need the invariant |
| `ON DELETE CASCADE` | app pattern: atomic block (small), `kevy-cli delete-prefix` (bulk), or a CDC consumer reacting to parent deletes (async — cookbook recipe 10) |
| triggers | **CDC consumers**: `FEED.READ` delivers every committed write as a change frame — after commit, decoupled, replayable, and it can't corrupt the write path (cookbook recipes 10–12) |

There is deliberately no server-side constraint or trigger DSL: Lua
scripts are the only server-side logic, and they run *as the write*,
not attached to it.

## Secondary index DDL

| SQL | kevy |
|---|---|
| `CREATE INDEX idx ON t (col)` | `IDX.CREATE idx ON PREFIX t: FIELD col TYPE i64\|f64\|str KIND range` |
| `CREATE UNIQUE INDEX` | `KIND unique` (fence semantics, above) |
| `CREATE INDEX … USING gin (tsvector)` | `KIND text` ([text-search](text-search.md)) |
| pgvector `USING hnsw` | `KIND ann DIM d [DISTANCE cosine\|l2\|ip] [M m] [EF ef]` ([vector-search](vector-search.md)) |
| `DROP INDEX` | `IDX.DROP idx` |
| `\d` / information_schema | `IDX.LIST` / `VIEW.LIST` (state, entries, bytes) |

- **Online build**: `IDX.CREATE` returns immediately and backfills in
  the background (~7s per million small rows; multi-KB text bodies
  ~85s/M). Queries answer `-INDEXBUILDING` until ready — poll
  `IDX.LIST` for `state=ready` ([migration](migration.md)). Data
  availability never waits on an index build.
- Index content is **derived state**: never snapshotted or AOF-logged,
  rebuilt in the background after restart. The catalog (the
  declarations) persists in a data-dir sidecar.
- Up to 64 indexes; `MAXMEM` caps a build declaratively
  (`-INDEXOVERBUDGET`) instead of growing unbounded.
- **Bulk-load rule**: import first, declare indexes after — backfill
  at bulk speed beats paying the write hook per imported row
  (cookbook recipe 15).

## Pagination and optimistic locking patterns

- **Keyset/cursor pagination** (the good kind of `OFFSET`): every
  paged surface (`IDX.QUERY RANGE/EQ/COMPOSE`, `VIEW.QUERY`, `SCAN`)
  returns `[next-cursor, rows]`; feed the cursor back until `"0"`.
  Treat cursors as opaque and single-traversal.
- **Version-column optimistic locking**: keep a `version` field,
  `WATCH` the row, read, `MULTI` + `HSET … version n+1` + `EXEC`; nil
  reply = lost the race, retry (cookbook recipe 4).

## Backup and PITR

| RDS | kevy |
|---|---|
| `mysqldump` / `pg_dump` | `kevy-cli export` (logical RESP stream, `redis-cli --pipe`-compatible) |
| binary backup | snapshot files (`SAVE`/`BGSAVE` → `dump-<id>.rdb`) |
| WAL / binlog | the AOF (append-only command log, per shard) |
| `synchronous_commit` | `appendfsync always` (or `everysec` + `fsync_aof` barriers) |
| PITR (base + WAL replay) | **recovery-point contract**: snapshot + CDC frames from the snapshot's recorded cursor = exact state at any later cursor ([persistence](persistence.md)) |

Scope note on PITR: the feed window is an in-memory backlog
(`feed_buffer_size`, caps 1 GiB/shard) — take snapshots at least as
often as the window turns over if you rely on exact-point recovery.
Verification is first-class: `PREFIX.DIGEST` / `kevy-cli diff` prove
two keyspaces equal, order- and topology-insensitively.

## Replication and read scaling

Primary + N replicas, asynchronous by default — the read-replica
shape. The differences from an RDS are all in the **consistency
ladder** ([availability](availability.md)), which prices each
guarantee per call instead of per instance:

| RDS notion | kevy rung |
|---|---|
| async replica (MySQL default) | rung 0: primary acks after local apply, replicas trail |
| session read-your-writes | rung 1: `REPL.TOKEN` on the primary → `REPL.WAIT token` on the replica → your next read observes your write |
| semi-sync replication | rung 2: `WAIT n timeout` per write — on the primary **and** acked by ≥ n replicas (ACK ≠ fsync) |
| max replica lag guard | rung 3: `replica_max_staleness_ms` — a stale replica answers `-STALE` instead of serving old reads |
| refuse writes without standbys | rung 4: `min_replicas_to_write` (`-NOREPLICAS`) |
| fencing / split-brain guard | rung 5: quorum lease — a partitioned primary fences its own writes |

Failover: planned handover is the `FAILOVER` verb (quiesce → drain →
promote — zero-loss); crash failover is the 3-node elect quorum
(MTTR ≈ 5–8s, offset-best candidate wins so `WAIT`-acked writes
survive). A rejoining ex-primary's un-replicated tail is **discarded**
(the forked suffix was never acked by definition) — the same
data-loss shape as losing un-shipped binlog in MySQL semi-sync, made
explicit. Every degraded state is a named error (`-READONLY`,
`-STALE`, `-NOREPLICAS`, `-QUIESCED`, `-MISDIRECTED`) that a routing
client self-heals from ([error-replies](error-replies.md)).

## CDC

The feed is kevy's binlog-as-API — what Debezium extracts from an RDS,
served natively ([cdc](cdc.md)):

| Debezium/binlog notion | kevy |
|---|---|
| binlog position / LSN | `(generation, offset)` cursor, per shard |
| connector snapshot + stream | rebuild (SCAN your prefix) + resume from `FEED.TAIL` |
| topic-per-table filtering | `PREFIX` filter on `FEED.READ` (fail-open for multi-key frames) |
| at-least-once delivery | same — consumers must be idempotent |
| transactional outbox pattern | **unnecessary**: every committed write already IS a change frame (cookbook recipe 11) |

Not provided, deliberately: global cross-shard order (per-key order is
guaranteed within a shard stream), server-side consumer positions,
consumer groups. Retention is the in-memory backlog; falling behind
answers `-FEEDRESYNC` with the cursor to rebuild from.

## What kevy will NOT do

The refusals, in one place. Each is a charter decision
([designing-on-kevy](designing-on-kevy.md), the project's scope log) —
not a roadmap gap:

| Refused | Because | Use instead |
|---|---|---|
| SQL parser / query DSL | the planner slope starts here | explicit `IDX.*`/`VIEW.*` + this matrix |
| query planner / auto index selection | engine must execute declared paths, not decide | `IDX.EXPLAIN` (diagnostic only) |
| JOINs | no server-side plan search | denormalize / hydrate (`FIELDS`, `VIA`) / view |
| `WHERE` without an index | accidental O(n) is a lie waiting to page you | declare the path or don't ship the query |
| server-side constraint DSL / triggers | stored app logic isn't engine-genre | Lua `EVAL` (atomic unit) + CDC consumers |
| DECIMAL type | no server-side arithmetic beyond i64/f64 | integer minor units |
| JSON-path queries | hash fields ARE the column model | flatten to fields (cookbook recipe 9) |
| `HAVING` / aggregate expressions | query-language slope | filter `GROUPS` output app-side |
| `OFFSET` pagination | O(N) skip is an anti-pattern | cursors |
| multi-database `SELECT n` | one keyspace per server | prefixes or separate instances |
| AUTH / TLS | trust boundary is the network perimeter | loopback bind (default), sidecar proxy |
| cross-DC active-active, CRDTs | single-DC charter | app-level federation |
| dynamic membership / auto-replace / resharding | topology is operator-declared | config + rolling restart |
| HTTP/REST API | RESP + MCP are the two access planes | any Redis client |

## Sizing and operational deltas

**Everything is RAM-resident.** The working set that an RDS pages from
disk lives in memory here; disk is the durability log + snapshot, not
the serving tier. Capacity planning in one line:

> RAM ≈ rows × (avg key len + avg row bytes + per-key overhead)
> + Σ per-index formula + view members × entry size — then verify
> against `MEMORY USAGE` / `IDX.LIST` bytes on a loaded sample.

The per-subsystem formulas (each gated against measured RSS in CI):
range index ≈ `rows × (value_width + avg_key_len + 48)`; text and ANN
formulas in [text-search](text-search.md) /
[vector-search](vector-search.md) (1M × 1024d vectors ≈ 4.1 GiB);
agg ≈ groups-dominated ([indexes](indexes.md)); view members ≈
`order_value_width + key_len + 48` ([views](views.md)). Set
`maxmemory` + an eviction policy or accept `-OOM` refusals (writes are
rejected at admission — existing data is never corrupted).

**Serving headroom.** The arena, re-measured 2026-07-19 on v4.0.0
(kevy vs valkey 9.1, fair-fight protocol, median-of-5, throughput read
from each server's own command counter — `bench/ARENA-2026-07-19.txt`):
GET 2.46×, SET 4.00×, INCR 2.86×, SADD 2.95×, HSET 2.17×, ZADD 1.78×,
LPUSH 1.76× — 7/7 wins with the full replication/heartbeat pipeline
landed. Some of these ratios are lower than the v3.18.0 figures they
replace, because that run read `redis-benchmark`'s own rate, which is
quantized low under `--threads`
(`bench/PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md`);
counting server-side raised every engine's number, the competitors'
by more. Against
a disk-first RDS on point reads the gap is larger still; the honest
comparison there is "kevy replaces the cache tier AND the operational
queries", not a per-query benchmark.

**Operational deltas vs an RDS**, in brief: single keyspace (no
schemas/databases — prefixes are the namespace); TTLs are first-class
(`EXPIRE`, per-field `HEXPIRE` — the cron-delete job disappears);
`FLUSHALL` is one command away (perimeter-protect it); derived state
(indexes, views, feeds) rebuilds after restart rather than loading
(seconds per million rows — budget it into restart time); and the
error surface is part of the contract — clients should match error
prefixes, not parse messages ([error-replies](error-replies.md)).

## This page vs the cookbook

This page is the **reference matrix** — SQL construct in, kevy shape
out, deltas stated. The [cookbook](cookbook.md) is the **recipe book**
— 20 runnable patterns (every command block CI-smoked), each tagged
with the SQL construct it replaces and cross-linked back to the matrix
rows above. Migrating for real? The phased playbook is in
[migration](migration.md).
