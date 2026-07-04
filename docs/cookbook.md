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

## 1. Tables and rows

A row is a hash under a typed prefix:

```
HSET user:42 name "ada" email "ada@example.com" age 36
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

```
order:1001                      # the row
user:42:orders     (SET)        # 1-N: member = order id
order:1001:items   (LIST/SET)
tag:urgent:orders  (SET)        # N-M: one set per side
order:1001:tags    (SET)
```

Or skip the link keys entirely: put the foreign key in the row
(`HSET order:1001 user_id 42`) and declare an index
(`IDX.CREATE order_user ON PREFIX order: FIELD user_id TYPE i64 KIND
range`) — `IDX.QUERY order_user EQ 42 FIELDS total status` is the
`SELECT … WHERE user_id = 42` of this world, hydrated in one hop.

## 3. Sequences

```
INCR seq:order                  # one id
INCRBY seq:order 100            # block allocation: hand out 100 ids
                                # from app memory, refill when dry
```

Block allocation is the high-throughput form; gaps on crash are the
same contract PostgreSQL sequences give you.

## 4. Optimistic locking (row versions)

Server: WATCH/MULTI — the CAS loop:

```
WATCH user:42
HGET user:42 version            # read, decide
MULTI
HSET user:42 balance 90 version 8
EXEC                            # nil reply = somebody won the race; retry
```

Embedded: run the read-decide-write inside one `atomic()` block —
the shard lock makes the branch race-free without a retry loop.

## 5. CHECK constraints and multi-key invariants

The RDS runs `CHECK (balance >= 0)` in the engine. kevy's replacement
is **reads inside the atomic block**: the app evaluates the
invariant, the engine guarantees the decision and the write commit
together.

```
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

```
IDX.CREATE req_idem ON PREFIX req: FIELD idem_key TYPE str KIND unique
```

Write the row, then `IDX.QUERY req_idem EQ <key>` — duplicates are
*visible* (the unique kind counts them in VERIFY rather than
rejecting writes; declarative fence, not a write gate). For a hard
gate use `SET idem:<key> 1 NX PX 86400000` before processing: NX is
the atomic claim, the TTL is the retention window.

## 7. Soft delete

Flag, don't remove:

```
HSET user:42 deleted 1
IDX.CREATE user_live ON PREFIX user: FIELD deleted TYPE i64 KIND range
IDX.QUERY user_live EQ 0 …      # live rows only
```

Views compose it away permanently: `VIEW.CREATE live_users QUERY
( AND user_live EQ 0 user_age RANGE 18 200 ) ORDER BY user_age` —
callers never re-state the filter.

## 8. Composite ordering (ORDER BY a, b)

Encode the composite into one indexed score field at write time:
`score = a * 1_000_000 + b` for bounded integer `b`, or a
zero-padded string field for lexicographic composites
(`"2026-07-04|000042"` with `TYPE str KIND range`). One index, one
ORDER BY; the write hook maintains it like any field.

## 9. JSONB

Flatten to hash fields: `profile.city` → field `profile.city`. You
keep per-field reads/writes, field TTLs (HEXPIRE), and indexability —
everything JSONB gave you except JSON-path queries, which are
**permanently out** (query-engine slope; see the REFUSED table in
docs/designing-on-kevy.md). A deeply nested blob nobody indexes can
stay one serialized field; the moment a path matters, promote it to
a field.

## 10. Cascade delete / foreign keys

Cascades are app patterns, never engine magic:

- Synchronous, small blast radius: delete inside one atomic block
  (`ctx.del(row)`, `ctx.srem(parent_link, id)`).
- Bulk / prefix-shaped: `kevy-cli delete-prefix --rate 5000 order:1001:` —
  rate-limited, resumable.
- Asynchronous: a CDC consumer (`FEED.READ` with `PREFIX`) reacts to
  parent deletes and cleans children — the trigger replacement, after
  commit, decoupled, replayable.

## 11. The outbox you don't need

The transactional-outbox pattern exists because an RDS commit and a
message-bus publish can't be atomic. In kevy **the feed is the
outbox**: every committed write is already a change frame at a
`(generation, offset)` cursor, at-least-once, prefix-filterable
(docs/cdc.md). Consume `FEED.READ`; don't build a second journal.

## 12. Audit history

CDC retention IS the audit log: frames carry the applied effect argv
in commit order. Size the feed backlog for the window you owe
compliance, export to cold storage with a cursor consumer. For
point-in-time reconstruction: restore snapshot + replay to the
`(gen, offset)` recovery point (docs/persistence.md).

## 13. The rollback window (reverse mirror)

During cutover, run a CDC consumer that mirrors kevy writes BACK to
the old RDS (`FEED.READ` → UPDATE statements). Your rollback plan is
then "repoint the app", not "reverse-migrate data". Decommission the
mirror when confidence hardens; `kevy-cli diff` (per-prefix digests)
is the confidence meter.

## 14. Analytics export

Serving and analytics don't share an engine. Export patterns:

- `kevy-cli export --prefix order: orders.resp` — logical, resumable,
  loadable anywhere RESP goes.
- CDC → warehouse: a cursor consumer streaming inserts to your OLAP
  store, exactly the CDC-to-Kafka shape.
- Read-only listener (`docs/embedded-listener.md`) for ad-hoc pulls
  from embedded apps.

## 15. Loading order (the deferred-index rule)

Bulk load FIRST, declare indexes/views AFTER: backfill builds from
existing rows at ~7s/million — orders of magnitude cheaper than
paying the write hook per imported row (docs/migration.md).
