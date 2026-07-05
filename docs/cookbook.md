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
(`kevy --port 6004`; recipes 11–14 and 16 additionally want
`[feed] enabled = true` in `kevy.toml` — see docs/cdc.md).
`bench/cookbook_smoke.sh` executes every `kevy-cli` line below
against a throwaway server, so the blocks stay honest.

## 1. Tables and rows

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

```console
kevy-cli -p 6004 INCR seq:order          # one id
kevy-cli -p 6004 INCRBY seq:order 100    # block allocation: hand out 100 ids
                                         # from app memory, refill when dry
```

Block allocation is the high-throughput form; gaps on crash are the
same contract PostgreSQL sequences give you.

## 4. Optimistic locking (row versions)

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

The RDS runs `CHECK (balance >= 0)` in the engine. kevy's replacement
is **reads inside the atomic block**: the app evaluates the
invariant, the engine guarantees the decision and the write commit
together.

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

Cascades are app patterns, never engine magic:

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
