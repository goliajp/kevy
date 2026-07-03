# CDC — the change feed (`FEED.*` / `changes_since`)

kevy exposes every applied write as a consumable change stream — the
same effect frames the AOF logs and replicas apply. Typical consumers:
cache invalidators, search-index updaters, downstream mirrors — the
jobs an "outbox table" does in an RDS stack, without the table.

Enable it in config:

```toml
[feed]
enabled = true
# feed_buffer_size = "64mb"   # per shard, caps at 1gb
```

or embedded: `Config::default().with_feed(0)`.

## The cursor: `(generation, offset)`

Every stream position is a `(generation, offset)` pair:

- `offset` increases by one per applied write within a generation.
- `generation` identifies one unbroken offset history. A given
  `(generation, offset)` refers to the same stream prefix **forever**.
- A **clean shutdown + restart preserves both** — consumers resume
  where they left off.
- Continuity breaks bump the generation and restart offsets at 0:
  `FLUSHALL`, restoring from a snapshot, or an **unclean shutdown**
  (crash). A bumped generation tells consumers their view may be
  stale: rebuild, then resume.

## Server surface

- `FEED.SHARDS` → `:N` — number of independent streams (one per
  shard; keys map to shards, so **per-key order is guaranteed** within
  a stream; there is no cross-shard order).
- `FEED.TAIL <shard>` → `*2 [:generation, :next_offset]` — the cursor
  a fresh consumer starts from.
- `FEED.READ <shard> <gen> <offset> [COUNT n] [PREFIX p ...]` →
  `*3 [:generation, :next_cursor, *frames]`, each frame
  `*2 [:offset, *argv]`. Poll it; an empty frame list means caught up.
  - `COUNT` caps frames per call (default 256, max 4096).
  - `PREFIX` (repeatable, OR) filters frames by key prefix
    **fail-open**: frames whose key layout isn't the plain
    single-key shape (multi-key `DEL`, `MSET`, `FLUSHALL`, …) are
    always delivered. Filtering never changes the cursor — you can
    change filters without resyncing.

### Errors

- `-FEEDRESYNC <gen> <tail>` — your cursor is unservable (older
  generation, or the offsets were evicted from the backlog). Rebuild
  your derived state (e.g. `SCAN` your prefix), then resume from the
  carried `(gen, tail)`.
- `-ERR feed cursor ahead of stream` — a cursor from the future;
  caller bug.

## Embedded surface

```rust
let store = kevy_embedded::Store::open(Config::default().with_feed(0))?;
let (gen, off) = store.changes_tail()?;             // start cursor
let batch = store.changes_since(gen, off, 256, &[b"user:"])?;
for change in &batch.changes { /* change.offset, change.argv */ }
let (gen, off) = batch.next;                        // resume here
```

`feed_shards()` reports 1 — the embedded write path serializes all
shards into one stream, so the same consumer loop works against both
surfaces. `FeedError::Resync { generation, tail }` mirrors
`-FEEDRESYNC`.

## Delivery semantics

**At-least-once.** After a resync-rebuild (and in some failover
shapes) you may see frames you already applied — consumers must be
idempotent (cache invalidation naturally is). Frames carry the applied
**effect** (e.g. a `ZINTERSTORE` appears as `DEL` + plain `ZADD`), so
applying them is deterministic regardless of source state.

**Retention** is the in-memory backlog (`feed_buffer_size` per shard):
fall behind further than the window and you get `-FEEDRESYNC`. There
is no on-disk archive — see the recovery-point contract below for the
durable story.

## Recovery-point contract (PITR)

Snapshots taken with the feed enabled record the stream cursor in
their header, frozen in the same no-append window as the snapshot
data:

> **snapshot S + the feed frames from S's cursor = the exact state at
> any later cursor.**

`kevy_persist::read_snapshot_cursor(path)` reads it back (`None` for
pre-v2.3 snapshots). A restore drill: load the snapshot into a fresh
data dir, then replay `FEED.READ` frames from the recorded cursor —
byte-exact state, verifiable with `PREFIX.STATS` / key dumps. The
drill script lives at `bench/diskgate.sh`'s restore line.

## Per-prefix stats

`PREFIX.STATS <prefix>` (server) / `Store::info_prefix` (embedded)
report live key + TTL'd key counts under a byte prefix — O(keyspace)
walk, meant for ops dashboards, not hot paths.

## What this is not

No global cross-shard order, no server-side consumer positions, no
consumer groups, no query predicates (a prefix is a byte filter). If
you need those, you are describing a message broker or an RDS — kevy's
feed stops at the event-horizon deliberately (see the three-laws
design note in the repository docs).
