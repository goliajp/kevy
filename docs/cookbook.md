# The RDS→kevy modeling cookbook

You are moving a relational data model onto kevy. Every recipe below
uses shipped primitives — no roadmap features, no "coming soon". Each
one names the RDS concept it replaces and the kevy pattern that
carries it.

The design stance behind all of them: **model the access paths, not
the schema**. An RDS lets you defer that decision to a query planner;
kevy makes you state it — and pays you back with microsecond pages
at serving time (`bench/VALIDATION-LEDGER.md` has the measured
numbers).

Every command block runs as-is against a fresh local kevy
(`kevy --port 6004`; recipes 11–14, 16 and 20 additionally want
`[feed] enabled = true` in `kevy.toml` — see docs/cdc.md).
`bench/cookbook_smoke.sh` executes every `kevy-cli` line below
against a throwaway server, so the blocks stay honest.

## 1. Tables and rows

**SQL equivalent:** `CREATE TABLE` + `SELECT col FROM t WHERE id = ?`
— [matrix: tables, rows, columns](rds-workloads.md#tables-rows-columns).

A row is a hash under a typed prefix:

```console
kevy-cli -p 6004 HSET user:42 name ada email ada@example.com age 36
kevy-cli -p 6004 HGET user:42 name
kevy-cli -p 6004 HGET user:42 phone    # NULL = absent field: already answers (nil)
```

- Table → key prefix (`user:`). Column → hash field. Primary key →
  the key itself.
- **NULL = absent field.** Don't store sentinel strings; `HGET` of a
  missing field already answers nil, and index specs treat a missing
  field as "row excluded" (visible in `IDX.VERIFY` counts).
- Column types are yours: kevy stores bytes. Declare types where they
  matter — at index creation (`TYPE i64|f64|str|vector`); coercion
  failures are counted, never silently indexed.

## 2. One-to-many, many-to-many

**SQL equivalent:** foreign-key columns + junction tables;
`SELECT … FROM orders WHERE user_id = ?` —
[matrix: JOIN](rds-workloads.md#join).

Link keys carry the relation, one set per side:

```console
kevy-cli -p 6004 HSET order:1001 user_id 42 total 1999 status shipped
kevy-cli -p 6004 HSET order:1002 user_id 42 total 550 status pending
kevy-cli -p 6004 SADD user:42:orders 1001 1002       # 1-N: member = order id
kevy-cli -p 6004 RPUSH order:1001:items sku-7 sku-9
kevy-cli -p 6004 SADD tag:urgent:orders 1001         # N-M: one set per side
kevy-cli -p 6004 SADD order:1001:tags urgent
```

Or skip the link keys entirely: put the foreign key in the row
(`user_id` above) and declare an index — `IDX.QUERY … EQ 42` is the
`SELECT … WHERE user_id = 42` of this world, hydrated in one hop:

```console
kevy-cli -p 6004 IDX.CREATE order_user ON PREFIX order: FIELD user_id TYPE i64 KIND range
kevy-cli -p 6004 IDX.QUERY order_user EQ 42 FIELDS total status
```

## 3. Sequences

**SQL equivalent:** `AUTO_INCREMENT` / `CREATE SEQUENCE` + `nextval()`
— [matrix: primary key, unique, auto_increment](rds-workloads.md#primary-key-unique-auto_increment).

```console
kevy-cli -p 6004 INCR seq:order          # one id
kevy-cli -p 6004 INCRBY seq:order 100    # block allocation: hand out 100 ids
                                         # from app memory, refill when dry
```

Block allocation is the high-throughput form; gaps on crash are the
same contract PostgreSQL sequences give you.

## 4. Optimistic locking (row versions)

**SQL equivalent:** `UPDATE t SET …, version = v+1 WHERE id = ? AND
version = v` (the version-column CAS) —
[matrix: transactions](rds-workloads.md#transactions).

Server: WATCH/MULTI — the CAS loop. The transaction is
connection-scoped, so it runs in one REPL session (here fed by a
heredoc):

```bash
kevy-cli -p 6004 HSET user:42 balance 100 version 7
kevy-cli -p 6004 <<'TXN'
WATCH user:42
HGET user:42 version
MULTI
HSET user:42 balance 90 version 8
EXEC
TXN
```

`EXEC` answers nil when somebody else touched `user:42` after the
`WATCH` — somebody won the race; re-read and retry.

Embedded: run the read-decide-write inside one `atomic()` block —
the shard lock makes the branch race-free without a retry loop.

## 5. CHECK constraints and multi-key invariants

**SQL equivalent:** `CHECK (balance >= 0)` + a trigger-maintained
audit row — [matrix: constraints and triggers](rds-workloads.md#constraints-and-triggers).

The RDS runs `CHECK (balance >= 0)` in the engine. kevy's replacement
is **reads inside the atomic block**: the app evaluates the
invariant, the engine guarantees the decision and the write commit
together.

Concretely: returning `Err` from the closure rolls back every write it
made — in memory and in the AOF — so a rejected transaction leaves no
trace, and you do not have to arrange your closure so that validation
precedes every write. (That guarantee is real as of 4.0; earlier
versions left a rejected closure's writes live in memory while
discarding their AOF frames, so a restart disagreed with the running
process.)

```rust
// embedded — debit that must not overdraw, plus an audit row:
store.atomic(b"acct:7", |ctx| {
    let bal: i64 = parse(ctx.hget(b"acct:7", b"balance")?);
    if bal < amount { return Err(Overdraw); }
    ctx.hset(b"acct:7", &[(b"balance", &(bal - amount))])?;
    ctx.rpush(b"acct:7:ledger", &[entry])?;
    Ok(())
})
```

Cross-shard invariants: `atomic_all_shards` (deterministic lock
order, documented deadlock exemption). Use sparingly — it is the
serializable-transaction hammer, and most invariants live under one
key prefix by design.

## 6. Idempotency keys

**SQL equivalent:** `UNIQUE INDEX` + `INSERT … ON CONFLICT DO NOTHING`
— [matrix: primary key, unique, auto_increment](rds-workloads.md#primary-key-unique-auto_increment).

```console
kevy-cli -p 6004 HSET req:9001 idem_key pay-2026-07-04-a77 amount 1999
kevy-cli -p 6004 IDX.CREATE req_idem ON PREFIX req: FIELD idem_key TYPE str KIND unique
kevy-cli -p 6004 IDX.QUERY req_idem EQ pay-2026-07-04-a77   # duplicates are visible as multi-hit reads
kevy-cli -p 6004 IDX.VERIFY req_idem                        # ...and counted here
kevy-cli -p 6004 SET idem:pay-2026-07-04-a77 1 NX PX 86400000
```

Write the row, then query — duplicates are *visible* (the unique
kind counts them in VERIFY rather than rejecting writes; declarative
fence, not a write gate). For a hard gate use the `SET … NX PX` form
before processing: NX is the atomic claim, the TTL is the retention
window.

## 7. Soft delete

**SQL equivalent:** a `deleted` flag column + a partial index / view
`WHERE deleted = 0` — [matrix: VIEW](rds-workloads.md#view).

Flag, don't remove:

```console
kevy-cli -p 6004 HSET user:42 deleted 0 age 36
kevy-cli -p 6004 HSET user:43 deleted 1 age 51
kevy-cli -p 6004 IDX.CREATE user_live ON PREFIX user: FIELD deleted TYPE i64 KIND range
kevy-cli -p 6004 IDX.QUERY user_live EQ 0 LIMIT 100    # live rows only
```

Views compose it away permanently — callers never re-state the
filter:

```console
kevy-cli -p 6004 IDX.CREATE user_age ON PREFIX user: FIELD age TYPE i64 KIND range
kevy-cli -p 6004 VIEW.CREATE live_users QUERY '(' AND user_live EQ 0 user_age RANGE 18 200 ')' ORDER BY user_age
kevy-cli -p 6004 VIEW.QUERY live_users LIMIT 10
```

## 8. Composite ordering (ORDER BY a, b)

**SQL equivalent:** `ORDER BY a, b` on a composite index —
[matrix: ORDER BY / LIMIT / OFFSET](rds-workloads.md#order-by--limit--offset).

Encode the composite into one indexed score field at write time:
`score = a * 1_000_000 + b` for bounded integer `b`, or a
zero-padded string field for lexicographic composites — one index,
one ORDER BY; the write hook maintains it like any field:

```console
kevy-cli -p 6004 HSET evt:1 ord '2026-07-04|000042'
kevy-cli -p 6004 HSET evt:2 ord '2026-07-04|000007'
kevy-cli -p 6004 HSET evt:3 ord '2026-07-05|000001'
kevy-cli -p 6004 IDX.CREATE evt_ord ON PREFIX evt: FIELD ord TYPE str KIND range
kevy-cli -p 6004 IDX.QUERY evt_ord RANGE '2026-07-04|000000' '2026-07-04|999999' LIMIT 100
```

## 9. JSONB

**SQL equivalent:** a JSON/JSONB column with generated-column indexes
— [matrix: type system](rds-workloads.md#type-system).

Flatten to hash fields: `profile.city` → field `profile.city`. You
keep per-field reads/writes, field TTLs (HEXPIRE), and indexability —
everything JSONB gave you except JSON-path queries, which are
**permanently out** (query-engine slope; see the REFUSED table in
docs/designing-on-kevy.md).

```console
kevy-cli -p 6004 HSET user:7 profile.city tokyo profile.plan pro
kevy-cli -p 6004 HGET user:7 profile.city
kevy-cli -p 6004 HEXPIRE user:7 3600 FIELDS 1 profile.plan   # per-field TTL survives the flattening
```

A deeply nested blob nobody indexes can stay one serialized field;
the moment a path matters, promote it to a field.

## 10. Cascade delete / foreign keys

**SQL equivalent:** `FOREIGN KEY … ON DELETE CASCADE` —
[matrix: constraints and triggers](rds-workloads.md#constraints-and-triggers).

Cascades are app patterns, never engine magic. If you are writing more
than one of these, read [§21](#21-derived-state-as-a-pure-function-of-the-row)
first — cascade, uniqueness and drift detection are one pattern, and
this section is a special case of it.


- Synchronous, small blast radius: delete inside one atomic block
  (`ctx.del(row)`, `ctx.srem(parent_link, id)`).
- Bulk / prefix-shaped: `delete-prefix` — rate-limited, resumable.
- Asynchronous: a CDC consumer (`FEED.READ` with `PREFIX`) reacts to
  parent deletes and cleans children — the trigger replacement, after
  commit, decoupled, replayable.

```console
kevy-cli -p 6004 HSET order:1001 user_id 42
kevy-cli -p 6004 RPUSH order:1001:items sku-7 sku-9
kevy-cli -p 6004 SADD order:1001:tags urgent
kevy-cli delete-prefix -p 6004 --rate 5000 order:1001:   # children gone, parent row stays
```

## 11. The outbox you don't need

**SQL equivalent:** the transactional-outbox table + relay worker —
[matrix: CDC](rds-workloads.md#cdc).

The transactional-outbox pattern exists because an RDS commit and a
message-bus publish can't be atomic. In kevy **the feed is the
outbox**: every committed write is already a change frame at a
`(generation, offset)` cursor, at-least-once, prefix-filterable
(docs/cdc.md). Consume `FEED.READ`; don't build a second journal.

```console
# needs [feed] enabled = true in kevy.toml (docs/cdc.md)
kevy-cli -p 6004 HSET order:9001 status paid
kevy-cli -p 6004 FEED.SHARDS
kevy-cli -p 6004 FEED.TAIL 0                             # a fresh consumer's starting cursor
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 10 PREFIX order:  # gen 1 = a fresh data dir's first generation
```

## 12. Audit history

**SQL equivalent:** a trigger-maintained audit/history table (or
binlog archaeology) — [matrix: CDC](rds-workloads.md#cdc).

CDC retention IS the audit log: frames carry the applied effect argv
in commit order. Size the feed backlog for the window you owe
compliance, export to cold storage with a cursor consumer. For
point-in-time reconstruction: restore snapshot + replay to the
`(gen, offset)` recovery point (docs/persistence.md).

```console
kevy-cli -p 6004 HSET acct:7 balance 100
kevy-cli -p 6004 HSET acct:7 balance 90
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 100 PREFIX acct:   # who set what, in commit order
```

## 13. The rollback window (reverse mirror)

**SQL equivalent:** reverse replication back to the old primary
during a cutover — [migration playbook, phase 5](migration.md#phase-5--write-cutover--the-rollback-window).

During cutover, run a CDC consumer that mirrors kevy writes BACK to
the old RDS (`FEED.READ` → UPDATE statements). Your rollback plan is
then "repoint the app", not "reverse-migrate data". Decommission the
mirror when confidence hardens; `kevy-cli diff` (per-prefix digests)
is the confidence meter.

```console
kevy-cli -p 6004 HSET user:42 name ada
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 10 PREFIX user:   # the mirror consumer's read loop
kevy-cli diff 127.0.0.1:6004 127.0.0.1:6004 user:        # digests match: safe form of the check
kevy-cli diff old-rds-mirror.internal:6379 127.0.0.1:6004 user:   # needs-external
```

## 14. Analytics export

**SQL equivalent:** the ETL job / binlog tap feeding the warehouse —
[matrix: CDC](rds-workloads.md#cdc).

Serving and analytics don't share an engine. Export patterns:

- `export` — logical, resumable, loadable anywhere RESP goes.
- CDC → warehouse: a cursor consumer streaming inserts to your OLAP
  store, exactly the CDC-to-Kafka shape.
- Read-only listener (`docs/embedded-listener.md`) for ad-hoc pulls
  from embedded apps.

```console
kevy-cli -p 6004 HSET order:1001 user_id 42 total 1999
kevy-cli export -p 6004 --prefix order: /tmp/orders.resp
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 100 PREFIX order:   # the CDC-to-warehouse read loop
```

## 15. Loading order (the deferred-index rule)

**SQL equivalent:** `LOAD DATA` first, `CREATE INDEX` after (the
bulk-load discipline) — [matrix: secondary index DDL](rds-workloads.md#secondary-index-ddl).

Bulk load FIRST, declare indexes/views AFTER: backfill builds from
existing rows at ~7s/million — orders of magnitude cheaper than
paying the write hook per imported row (docs/migration.md).

```console
kevy-cli -p 6004 HSET item:1 price 10
kevy-cli -p 6004 HSET item:2 price 25
kevy-cli -p 6004 HSET item:3 price 7
kevy-cli export -p 6004 --prefix item: /tmp/items.resp
kevy-cli import -p 6004 /tmp/items.resp   # bulk load FIRST: no index write hook to pay
kevy-cli -p 6004 IDX.CREATE item_price ON PREFIX item: FIELD price TYPE i64 KIND range   # declare AFTER: backfill
kevy-cli -p 6004 IDX.QUERY item_price RANGE 0 100 LIMIT 10
```

---

The last three recipes swap the workload: not an RDS being replaced
but an AI agent's memory stack. Nothing new is needed — session
state, episodic memory and RAG retrieval are the same access-path
patterns wearing different key prefixes.

## 16. Session context with TTL

**SQL equivalent:** a sessions table + the expiry cron job —
[matrix: operational deltas](rds-workloads.md#sizing-and-operational-deltas).

An agent's working context is a row with a lease: the compacted
conversation lives in a hash, `EXPIRE` is the idle-eviction policy
(renewed every turn — a sliding window), and the feed is the audit
trail you replay when someone asks what the agent knew at turn 7.

```console
# needs [feed] enabled = true in kevy.toml (docs/cdc.md)
kevy-cli -p 6004 HSET session:a7 user 42 turns 6 messages 'wants refund for order 1001; tone calm' last_tool order_lookup
kevy-cli -p 6004 EXPIRE session:a7 3600
kevy-cli -p 6004 HSET session:a7 turns 7 messages 'refund approved; awaiting confirmation'
kevy-cli -p 6004 EXPIRE session:a7 3600                       # renew the lease on every turn
kevy-cli -p 6004 FEED.TAIL 0                                  # audit cursor: where the log ends now
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 100 PREFIX session:    # gen 1 = a fresh data dir's first generation
```

The `messages` field holds whatever summary your compaction step
produces; rewriting it is one `HSET`, and every revision is already
a change frame in commit order — the "conversation history" table
most agent frameworks bolt on is recipe 12's audit log for free.

## 17. Episodic memory (time × semantic)

**SQL equivalent:** `WHERE ts BETWEEN …` + pgvector
`ORDER BY embedding <=> ? LIMIT k` — [matrix: SELECT](rds-workloads.md#select).

Episodic memory answers two questions about the same rows: *what
happened recently* (time) and *what resembles this* (meaning). One
prefix, one index per question — `DIM 8` keeps the demo readable;
real embeddings are 768+ dimensions shipped as f32-LE blobs, and the
`csv:` debug form below is accepted everywhere a vector is (stored
fields and query vectors go through the same parser —
docs/vector-search.md).

```console
kevy-cli -p 6004 HSET mem:1 ts 1783200000 kind obs what 'user prefers dark roast' v csv:0.9,0.1,0,0,0,0,0,0
kevy-cli -p 6004 HSET mem:2 ts 1783203600 kind obs what 'user asked about decaf' v csv:0.8,0.3,0.1,0,0,0,0,0
kevy-cli -p 6004 HSET mem:3 ts 1783207200 kind reflection what 'coffee questions cluster in the morning' v csv:0,0.2,0.9,0.1,0,0,0,0
kevy-cli -p 6004 IDX.CREATE mem_ts ON PREFIX mem: FIELD ts TYPE i64 KIND range
kevy-cli -p 6004 IDX.CREATE mem_kind ON PREFIX mem: FIELD kind TYPE str KIND range
kevy-cli -p 6004 IDX.CREATE mem_ann ON PREFIX mem: FIELD v TYPE vector KIND ann DIM 8
kevy-cli -p 6004 IDX.QUERY mem_ts RANGE 1783203000 1783210000 LIMIT 10 FIELDS what      # recent memories
kevy-cli -p 6004 IDX.QUERY mem_ann KNN csv:0.85,0.2,0,0,0,0,0,0 LIMIT 2 FIELDS what ts  # similar memories
kevy-cli -p 6004 IDX.QUERY COMPOSE AND mem_ts RANGE 1783203000 1783210000 mem_kind EQ reflection LIMIT 10 FIELDS what
```

`COMPOSE AND` conjoins scalar legs (`RANGE`/`EQ`) — here "in this
time window AND a reflection". For *similar within a window* there
is deliberately no KNN leg (filtering inside the graph walk is the
query-engine slope, REFUSED): run the KNN with headroom on `LIMIT`,
hydrate `ts` via `FIELDS` as above, and drop out-of-window hits
client-side.

## 18. RAG chunks with hybrid retrieval

**SQL equivalent:** tsvector full-text + pgvector KNN, fused
app-side — [matrix: SELECT](rds-workloads.md#select).

Chunks are rows carrying both retrieval surfaces — the text and its
embedding — so one write maintains both indexes:

```console
kevy-cli -p 6004 HSET chunk:1 doc kevy-guide seq 1 body 'rows are hashes under a typed key prefix' v csv:1,0,0,0,0,0,0,0
kevy-cli -p 6004 HSET chunk:2 doc kevy-guide seq 2 body 'indexes are declared once and maintained by the write hook' v csv:0,1,0,0,0,0,0,0
kevy-cli -p 6004 HSET chunk:3 doc kevy-guide seq 3 body 'the feed streams every committed write as a change frame' v csv:0,0,1,0,0,0,0,0
kevy-cli -p 6004 IDX.CREATE chunk_text ON PREFIX chunk: FIELD body TYPE str KIND text
kevy-cli -p 6004 IDX.CREATE chunk_ann ON PREFIX chunk: FIELD v TYPE vector KIND ann DIM 8
kevy-cli -p 6004 IDX.QUERY HYBRID chunk_text MATCH 'typed key prefix' chunk_ann KNN csv:0.9,0.1,0.1,0,0,0,0,0 LIMIT 2 FIELDS body
kevy-cli -p 6004 IDX.QUERY HYBRID chunk_text MATCH 'change frame' chunk_ann KNN csv:0,0.1,0.9,0,0,0,0,0 LIMIT 2 RRFK 20 FIELDS body
```

`HYBRID` runs both legs server-side and fuses by **reciprocal-rank
fusion**: each key scores `Σ 1/(k + rank)` across the BM25 list and
the KNN list — rank-only, so the two heterogeneous score scales
never need normalizing, and a chunk near the top of *both* legs
beats a chunk that tops only one. `RRFK` is the k (default 60):
lower it when you trust each leg's top hits and want agreement there
to dominate; raise it to flatten the fusion toward consensus deeper
in both lists.

---

The last two recipes leave the rack entirely: a kevy on an edge node
— the same server binary, or `kevy-embedded` compiled down to its
`core` tier at 655 KB ([docs/iot.md](iot.md)) — speaks the same
verbs, so the patterns transfer verbatim from datacenter to sensor
gateway.

## 19. Sensor cache (latest value + liveness lease)

**SQL equivalent:** the `readings_latest` upsert table plus the
staleness cron —
[matrix: operational deltas](rds-workloads.md#sizing-and-operational-deltas).

The current value of every sensor is a row; the TTL is the liveness
contract. A sensor that stops reporting expires out of the cache —
**absence IS the offline signal**, no reaper job to write:

```console
kevy-cli -p 6004 HSET sensor:t1 val 21.5 unit C ts 1783200000
kevy-cli -p 6004 EXPIRE sensor:t1 90
kevy-cli -p 6004 HSET sensor:t1 val 21.7 unit C ts 1783200030
kevy-cli -p 6004 EXPIRE sensor:t1 90      # every report renews the lease
kevy-cli -p 6004 EXISTS sensor:t1         # 1 = reporting, 0 = gone dark
```

Size the lease to your alarm tolerance (here 90 s = three missed
30-second reports). To *react* to a sensor going dark instead of
polling, enable keyspace notifications with the `x` (expired) class
and subscribe to the expiry events — the push form of the same
contract (docs/pubsub.md).

The recent window is a stream with a hard cap — `MAXLEN ~` keeps the
node's memory bounded no matter how long it runs, which on a
months-uptime edge box is the invariant that matters:

```console
kevy-cli -p 6004 XADD sensor:t1:log MAXLEN '~' 1000 '*' val 21.5
kevy-cli -p 6004 XADD sensor:t1:log MAXLEN '~' 1000 '*' val 21.7
kevy-cli -p 6004 XLEN sensor:t1:log
kevy-cli -p 6004 XRANGE sensor:t1:log - + COUNT 10
```

Embedded form: same verbs through the typed API inside your gateway
process — `store.hset(…)` / `store.expire(…)` / `store.xadd(…)` —
with no socket at all; the `core` feature tier carries everything
this recipe uses (docs/iot.md).

## 20. Edge aggregation (write-time GROUP BY + uplink)

**SQL equivalent:** `SELECT zone, COUNT(*), SUM(w) … GROUP BY zone`
re-run per dashboard refresh —
[matrix: GROUP BY and aggregates](rds-workloads.md#group-by-and-aggregates).

An edge node summarizes locally and ships summaries — raw readings
are too many to uplink. Declare the aggregate once; it is maintained
in the write path, so the "aggregation job" simply stops existing:

```console
kevy-cli -p 6004 HSET reading:1 zone floor1 w 120
kevy-cli -p 6004 HSET reading:2 zone floor1 w 180
kevy-cli -p 6004 HSET reading:3 zone floor2 w 95
kevy-cli -p 6004 IDX.CREATE zone_w ON PREFIX reading: FIELD w TYPE i64 KIND agg GROUPBY zone
kevy-cli -p 6004 IDX.QUERY zone_w GROUP floor1            # [count, sum, min, max, avg]
kevy-cli -p 6004 IDX.QUERY zone_w GROUPS BY sum LIMIT 10  # zones ranked by load
```

The uplink is recipe 11's outbox wearing overalls: the feed already
journals every committed write, so the cloud-sync consumer is a
cursor loop, resumable across links that drop for hours —
at-least-once, in commit order, prefix-filtered to just what the
cloud needs:

```console
# needs [feed] enabled = true in kevy.toml (docs/cdc.md)
kevy-cli -p 6004 FEED.TAIL 0
kevy-cli -p 6004 FEED.READ 0 1 0 COUNT 100 PREFIX reading:   # the uplink loop
```

Pair it with recipe 19's `MAXLEN` cap and TTLs: raw readings stay
bounded on the node, the aggregate rows stay tiny, and the feed
cursor survives reboots — the whole edge story with zero moving
parts beyond kevy itself.

## 21. Derived state as a pure function of the row

**SQL equivalent:** the whole trigger layer at once — `ON DELETE
CASCADE`, `UNIQUE` constraints, and the reconciliation job you write
after you stop trusting them —
[matrix: constraints and triggers](rds-workloads.md#constraints-and-triggers).

This is the pattern the recipes above keep circling: §2's link keys,
§5's invariants, §10's cascades and §12's audit rows are all the same
idea applied four times. Stated once, it resolves cascades, uniqueness
and drift detection together. It comes from a production migration that
took a day to arrive at it, which is a day worth saving.

**The idea:** write one pure function from a row to every key derived
from it. Not a procedure that updates keys — a function that *returns*
what should exist.

```rust
// Everything user:42 implies, computed from the row alone.
fn derived(id: &[u8], row: &Row) -> Vec<Vec<u8>> {
    vec![
        key(b"email:", &row.email),          // uniqueness claim
        key(b"dept:", &row.dept, b":users"), // membership
    ]
}
```

Every operation is then a diff, and each falls out rather than being
designed:

| Operation | What you do | What you get for free |
|---|---|---|
| **Insert** | add `derived(new)` | claims and memberships appear together |
| **Update** | add `derived(new) - derived(old)`, remove `derived(old) - derived(new)` | a renamed email **releases its old claim** — the bug everyone writes by hand |
| **Delete** | remove `derived(old)` | the cascade is not a separate code path |
| **Verify** | recompute `derived` for every row, diff against what exists | a drift detector you did not have to design |

The update row is the one that pays for the pattern. Hand-written
cascade code almost always adds the new claim and forgets to release the
old one, because release is the case nobody demonstrates in a ticket.

```rust
store.atomic_all_shards(|ctx| {
    let old = read_row(ctx, id)?;
    let (want, had) = (derived(id, &new), derived(id, &old));

    for k in want.iter().filter(|k| !had.contains(k)) {
        if ctx.exists(&[k]) > 0 { return Err(Taken); }  // uniqueness
        ctx.set(k, id);
    }
    for k in had.iter().filter(|k| !want.contains(k)) {
        ctx.del(&[k]);                                  // release
    }
    write_row(ctx, id, &new)
})
```

Returning `Err` rolls the whole thing back (§5), so a rejected write
leaves neither the row nor a half-applied claim set.

**Claims or an index?** A uniqueness claim is a second source of truth
that can drift from the rows; a [secondary index](indexes.md) is
derived by construction and cannot. Inside `atomic_all_shards` you can
query one directly:

```rust
if ctx.idx_count(b"email_idx", &want, &want)? > 0 { return Err(Taken); }
```

Two limits, both deliberate:

- **Only on `atomic_all_shards`.** An index entry lives on the shard of
  the key it indexes, so "does any row have this email" is a question
  about every shard. Single-shard `atomic()` holds one lock and could
  answer only for its own slice — a uniqueness check that consults 1/N
  of the keyspace would report "unique" nearly always, so it is not
  offered rather than offered with a footnote.
- **Index reads do not see the transaction's own writes.** Maintenance
  runs at commit. A closure inserting two rows must compare them to each
  other itself.

**Verification.** Because `derived` is a function, the checker *is* the
function — pass the same one to `reconcile`:

```rust
let report = store.snapshot().reconcile(
    b"user:",                    // the rows
    &[b"email:", b"dept:"],      // where derived keys live
    |key, row| derived(key, row),
);
if !report.is_clean() {
    warn!("{} missing, {} orphaned", report.missing_count, report.orphaned_count);
}
```

It diffs **both** directions, which is the part worth not writing
yourself. A missing key is lost derived state; an *orphan* — a claim
whose row is gone — is what a half-applied update leaves behind, and it
is the failure that silently blocks a later insert. A checker that only
looks for absences reports "clean" during exactly that failure.

It runs against a snapshot (`store.snapshot()`), frozen under every
shard lock, so it does not mistake a concurrent write for drift. That
holds only if the write itself was atomic: a row and its claims must go
in one `atomic_all_shards` block, or there is a real half-applied state
to find. Reconciliation and atomic writes are the same guarantee seen
from two ends.

Run it at boot, or on a schedule, or never once you trust the writes —
but run it, because it is the only thing that can tell you the invariant
you believe in is the invariant you have.

## 22. Porting a PG/MySQL schema

**SQL equivalent:** the schema file itself — `CREATE TABLE`,
`CREATE INDEX`, `CREATE VIEW` — [matrix: secondary index
DDL](rds-workloads.md#secondary-index-ddl).

Everything recipes 1–8 do by hand, compiled from the SQL you already
have. `kevy-sql` (and its `kevy-cli sql` face) is a **declaration-time
compiler**: it reads the schema ONCE, like a migration tool, and emits
explicit `TABLE.DECLARE` / `VIEW.CREATE` commands plus *query cards* —
ready-made `IDX.QUERY` templates with `$N` slots. Nothing runs
per-query inside the server; ad-hoc runtime SQL stays refused by the
engine itself (Law 3).

The schema — [docs/examples/shop.sql](https://github.com/goliajp/kevy/blob/main/docs/examples/shop.sql), a real
users/orders/order_items cut-down:

```sql
CREATE TABLE users (
  id     bigserial PRIMARY KEY,
  email  text,
  name   text,
  plan   text
);
CREATE UNIQUE INDEX ON users (email);

CREATE TABLE orders (
  id          bigserial PRIMARY KEY,
  user_id     bigint,
  status      text,
  total       numeric(10,2),
  created_at  bigint       -- epoch seconds, app-encoded
);
-- INCLUDE = PG covering columns -> kevy stored VALUES (residual FILTER/SORT).
CREATE INDEX ON orders (status) INCLUDE (total, created_at);
-- Multi-column -> a composite ORDERPATH (the (user_id, created_at DESC) walk).
CREATE INDEX ON orders (user_id, created_at DESC);

CREATE TABLE order_items (
  id        bigserial PRIMARY KEY,
  order_id  bigint,
  sku       text,
  qty       int
);
CREATE INDEX ON order_items (order_id);

CREATE VIEW paid_orders AS
  SELECT * FROM orders WHERE status = 'paid';

CREATE VIEW recent_orders_by_user AS
  SELECT id, status, total, created_at FROM orders
  WHERE user_id = $1
  ORDER BY created_at DESC
  LIMIT 20;
```

Compile it, then apply the declarations to a server:

```console
kevy-cli sql compile docs/examples/shop.sql
kevy-cli sql compile docs/examples/shop.sql --apply --url 127.0.0.1:6004
```

The compiled script (verbatim). Each table folds its indexes into one
`TABLE.DECLARE`; the constant view becomes an engine view; the
parameterized view becomes a query card; every coarse type mapping is
called out honestly in the notes (kevy columns are `i64|f64|str` —
timestamps are app-encoded, `serial` does not allocate ids for you):

```text
TABLE.DECLARE users PREFIX users: PK id COLUMN id i64 COLUMN email str COLUMN name str COLUMN plan str INDEX email unique
TABLE.DECLARE orders PREFIX orders: PK id COLUMN id i64 COLUMN user_id i64 COLUMN status str COLUMN total f64 COLUMN created_at i64 INDEX status range VALUES total created_at ORDERPATH user_id_created_at ON user_id THEN created_at DESC
TABLE.DECLARE order_items PREFIX order_items: PK id COLUMN id i64 COLUMN order_id i64 COLUMN sku str COLUMN qty i64 INDEX order_id range
VIEW.CREATE paid_orders QUERY orders.status EQ paid ORDER BY orders.status

# ---- query card: recent_orders_by_user ----
# runtime template — substitute the $N slots and send as-is:
#   $1 = user_id (i64)
#   IDX.QUERY orders.user_id_created_at WHERE user_id EQ $1 LIMIT 20 FIELDS id status total created_at

# notes:
#   - users.id: bigserial → i64, but ids do NOT auto-increment — allocate them app-side (INCR block, cookbook §3)
#   - orders.total: numeric → f64 — fixed-point precision becomes binary float; keep money as integer cents if exactness matters
#   - view paid_orders: read with VIEW.QUERY paid_orders, then hydrate rows with HMGET <key> id user_id status total created_at
```

Rows are ordinary hashes under the table prefix (recipe 1), and the
compiled paths serve immediately — the card runs with a real argument
in the `$1` slot:

```console
kevy-cli -p 6004 HSET users:1 id 1 email ada@example.com name Ada plan pro
kevy-cli -p 6004 HSET orders:1 id 1 user_id 1 status paid total 19.5 created_at 1700000100
kevy-cli -p 6004 HSET orders:2 id 2 user_id 1 status pending total 5 created_at 1700000200
kevy-cli -p 6004 HSET orders:3 id 3 user_id 2 status paid total 8 created_at 1700000300
kevy-cli -p 6004 HSET order_items:1 id 1 order_id 1 sku sku-7 qty 2
kevy-cli -p 6004 IDX.QUERY users.email EQ ada@example.com
kevy-cli -p 6004 IDX.QUERY orders.user_id_created_at WHERE user_id EQ 1 LIMIT 20 FIELDS id status total created_at
kevy-cli -p 6004 VIEW.QUERY paid_orders LIMIT 10
kevy-cli -p 6004 IDX.QUERY orders.status EQ paid FILTER total RANGE 10 inf
kevy-cli -p 6004 IDX.QUERY order_items.order_id EQ 1 FIELDS sku qty
kevy-cli -p 6004 TABLE.LIST
```

- The card query is `SELECT id, status, total, created_at FROM orders
  WHERE user_id = 1 ORDER BY created_at DESC LIMIT 20` — served by the
  composite walk, newest first, hydrated in one hop.
- The `FILTER total RANGE 10 inf` line is a residual predicate over the
  `INCLUDE`d columns — `WHERE status = 'paid' AND total >= 10` without
  touching a row.
- `order_items.order_id EQ 1` is the FK lookup that replaces the JOIN
  (recipe 2): two queries, no query-time join.

**The refusals teach.** The compiler refuses everything that would need
query-time evaluation — by name, with line/column, pointing at the
recipe that models it. A JOIN:

```sql
CREATE VIEW order_emails AS
  SELECT id, email FROM orders
  JOIN users ON users.id = orders.user_id;
```

```text
$ kevy-cli sql compile join.sql
kevy-cli sql: join.sql: line 6, col 3: JOIN is not compilable — kevy
refuses query-time joins (Law 3); model the lookup with an indexed FK
column (IDX.QUERY t.fk EQ …) or app-side assembly (cookbook §2)
```

A view whose WHERE matches no declared access path errors naming the
exact declaration to add (`… matches no declared access path — add:
CREATE INDEX ON orders (status, total)`), and ad-hoc SQL at runtime
never had a door:

```text
$ kevy-cli -p 6004 SQL SELECT * FROM users
(error) ERR unknown command 'SQL'
```

## Recipe index

Recipe ↔ the SQL construct it replaces ↔ the
[rds-workloads.md](rds-workloads.md) matrix row that states the
semantics and limits.

| # | Recipe | SQL construct | Matrix row |
|---|---|---|---|
| 1 | Tables and rows | `CREATE TABLE`, point `SELECT` | [tables, rows, columns](rds-workloads.md#tables-rows-columns) |
| 2 | One-to-many, many-to-many | FK columns, junction tables, `WHERE fk = ?` | [JOIN](rds-workloads.md#join) |
| 3 | Sequences | `AUTO_INCREMENT` / `nextval()` | [PK, UNIQUE, AUTO_INCREMENT](rds-workloads.md#primary-key-unique-auto_increment) |
| 4 | Optimistic locking | version-column CAS `UPDATE` | [transactions](rds-workloads.md#transactions) |
| 5 | CHECK constraints | `CHECK (…)` + audit trigger | [constraints and triggers](rds-workloads.md#constraints-and-triggers) |
| 6 | Idempotency keys | `UNIQUE INDEX` + `ON CONFLICT DO NOTHING` | [PK, UNIQUE, AUTO_INCREMENT](rds-workloads.md#primary-key-unique-auto_increment) |
| 7 | Soft delete | flag column + filtered view | [VIEW](rds-workloads.md#view) |
| 8 | Composite ordering | `ORDER BY a, b` | [ORDER BY / LIMIT / OFFSET](rds-workloads.md#order-by--limit--offset) |
| 9 | JSONB | JSON column + generated-column indexes | [type system](rds-workloads.md#type-system) |
| 10 | Cascade delete / FKs | `ON DELETE CASCADE` | [constraints and triggers](rds-workloads.md#constraints-and-triggers) |
| 11 | The outbox you don't need | transactional-outbox table | [CDC](rds-workloads.md#cdc) |
| 12 | Audit history | audit table / binlog archaeology | [CDC](rds-workloads.md#cdc) |
| 13 | The rollback window | reverse replication at cutover | [migration playbook](migration.md#phase-5--write-cutover--the-rollback-window) |
| 14 | Analytics export | ETL / binlog tap to warehouse | [CDC](rds-workloads.md#cdc) |
| 15 | Loading order | bulk `LOAD DATA`, index after | [secondary index DDL](rds-workloads.md#secondary-index-ddl) |
| 16 | Session context with TTL | sessions table + expiry cron | [operational deltas](rds-workloads.md#sizing-and-operational-deltas) |
| 17 | Episodic memory | time `BETWEEN` + pgvector KNN | [SELECT](rds-workloads.md#select) |
| 18 | RAG hybrid retrieval | tsvector + pgvector, fused | [SELECT](rds-workloads.md#select) |
| 19 | Sensor cache | upsert table + staleness cron | [operational deltas](rds-workloads.md#sizing-and-operational-deltas) |
| 20 | Edge aggregation | `GROUP BY` per refresh + ETL uplink | [GROUP BY and aggregates](rds-workloads.md#group-by-and-aggregates) |
| 21 | Derived state as a function of the row | the trigger layer entire: cascades, `UNIQUE`, reconciliation | [constraints and triggers](rds-workloads.md#constraints-and-triggers) |
| 22 | Porting a PG/MySQL schema | `CREATE TABLE` / `CREATE INDEX` / `CREATE VIEW`, compiled | [secondary index DDL](rds-workloads.md#secondary-index-ddl) |
