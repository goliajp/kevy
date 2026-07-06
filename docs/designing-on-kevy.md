# Designing your application on kevy

kevy v3 is a **serving engine**: the primary data store for
applications that would otherwise put their operational model on a
relational database and cache in front of it. This page is the map —
what the engine gives you, the laws that bound it, and where each
capability lives.

If you're migrating from an RDS, read this page first, then work
through [the cookbook](cookbook.md) recipe by recipe.

## The seven planes

| Plane | What it carries | Where |
|---|---|---|
| **P0 — Operations** | Every op on every surface: server (RESP), embedded (in-process), Lua, atomic blocks, pipelines. One OP_TABLE, CI-enforced parity. | docs/verb-reference.md |
| **P1 — Atomicity & durability** | Single-shard atomic blocks, deterministic-order all-shard blocks, appendfsync × atomic-commit matrix, per-block fsync barriers. | docs/persistence.md |
| **P2 — Indexes** | Declared secondary indexes, four kinds: `range`, `unique`, `text` (CJK bigram + BM25), `ann` (HNSW). Derived-by-construction (write-hook maintained, zero drift), one-hop hydration (`FIELDS`), backfill rebuild. | docs/indexes.md, docs/text-search.md, docs/vector-search.md |
| **P3 — Views & algebra** | Named compositions over indexes (virtual / materialized top-K), zset/set algebra with full Redis semantics. | docs/views.md |
| **P4 — Flow** | CDC feed with `(generation, offset)` cursors (the built-in outbox), blocking pops, hash-field TTLs, snapshot read views, the embedded read-only RESP listener. | docs/cdc.md, docs/embedded-listener.md |
| **P5 — Evidence** | Every declared line measured and reconciled; crash-consistency chaos gates; 30M-key mixed-stack soak. | bench/VALIDATION-LEDGER.md |
| **P6 — Availability** | Replication with acked-offset truth and heartbeats, planned handover (`FAILOVER`) and crash failover (quorum election, election-only write authority, fork discard), and the opt-in consistency ladder: `WAIT`, read-your-writes tokens (`REPL.TOKEN`/`REPL.WAIT`), bounded staleness (`-STALE`), quorum-lease write fencing. 12 executable clamps (availgate) run in CI. | docs/availability.md, docs/replication.md |

## The three laws

The constitution that kept — and keeps — kevy from sliding into
being a worse RDS:

**Law 1 — The Redis contract is inviolable.** Every Redis op kevy
implements behaves exactly like Redis. New-genre capability never
changes the semantics of an existing verb, never leaks internal
keyspace into `SCAN`/`KEYS`/`DBSIZE`, and lives in its own command
namespaces (`IDX.*`, `VIEW.*`, `FEED.*`, `PREFIX.*`).

**Law 2 — Superset only when it is engine-genre.** A capability gets
in only if it is derived data with a lifecycle the engine can own:
declare → maintain → verify → rebuild. Indexes, views, feeds and
digests all pass that test. Stored app logic doesn't.

**Law 3 — The RDS event horizon.** No query language, no planner, no
joins, no server-side validation, no triggers. The moment the engine
starts *deciding* how to answer a question instead of *executing a
declared access path*, it's an RDS — a worse one. kevy stays on this
side of the horizon permanently.

## Considered and refused

Each of these has a covered answer — the need is real, the feature
is the wrong shape:

| You might ask for | Use instead |
|---|---|
| SQL / query DSL | explicit index APIs + [cookbook](cookbook.md) |
| Query planner / auto index selection | `IDX.EXPLAIN` (diagnostic only) |
| Joins | one-hop hydration (`FIELDS`, `VIA`) + app-side assembly |
| Foreign keys / cascades | atomic blocks, prefix bulk ops, CDC consumers |
| CHECK constraints | reads inside atomic blocks (app evaluates, engine commits atomically) |
| Schema / typed columns | bytes are yours; types declared at index creation |
| JSON-path queries | hash fields ARE the column model |
| Triggers / write-path UDFs | CDC consumers — after commit, decoupled, replayable |
| GROUP BY / aggregation pipelines | maintain aggregates at write time in your model |
| Time travel | snapshots + CDC retention (recovery-point contract) |
| Transactional outbox | unnecessary — the feed IS the outbox |
| WHERE without an index | deliberately absent: model the access path or don't ship it |

## The serving charter (what's gated, forever)

Numbers are ratchets — floors only rise. The standing lines
(measured values in `bench/VALIDATION-LEDGER.md`):

- Redis-parity throughput: 7-angle perfgate, floor = baseline×0.92.
- Hydrated row-list page p99 < 1ms; view page < 1ms; write fan-out
  through index+view hooks p99 < 200µs — on one server carrying the
  full stack.
- IDX.QUERY p99 < 2ms @ 1M rows; MATCH p95 < 20ms @ 1M docs;
  KNN p95 < 30ms with recall@10 ≥ 0.90 @ 1M×128d.
- Crash honesty: kill -9 mid-write → replay → derived state identical
  to a fresh rebuild. Restore point = snapshot + `(gen, offset)`.
- Empty catalogs cost nothing: every subsystem gates a zero-tax line.

## Where to start

1. Model your prefixes and access paths ([cookbook §1-2](cookbook.md)).
2. Bulk load, then declare indexes ([migration guide](migration.md)).
3. Compose views for your hot lists ([views](views.md)).
4. Wire your event consumers to the feed ([CDC](cdc.md)).
5. Verify with digests, watch the gates.
6. When one node stops being enough, add replicas and pick your rung
   on the consistency ladder ([availability](availability.md)).
