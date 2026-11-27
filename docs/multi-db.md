# RFC: multi-database support (`SELECT`, `databases N`)

Status: Phases 1–3 implemented (multi-DB, persistence, replication + MOVE). Cluster mode stays single-db by design.
Branch: `feat/multi-db`.

## Why

kevy today is single-database: `SELECT 0` is acknowledged for drop-in
compatibility and every other index is rejected (`dispatch.rs`). Redis and
Valkey default to `databases = 16`, and real clients rely on it — most
notably **Gitea**, which uses DB 0 for cache and DB 1 for sessions. Running
kevy as a drop-in replacement currently forces two kevy instances just for
Gitea.

## Design

### Core model

One `Store` per logical db **per shard**: `Shard.store: DbStores` where
`DbStores { stores: Vec<Store>, cur: Cell<u16> }`. Stores are created eagerly
(an empty keyspace allocates no table) and stamped with their `db_index`
(`Store::db_index`), which keyspace notifications use for the
`__keyevent@<db>__:*` channel names.

A shard is single-threaded, so the "current db" is a `Cell<u16>` cursor:

- set from the connection at the top of `handle_command` (`conn.db`),
- carried across shards inside `DispatchMeta.db` (single-key batch lane) and
  `Inbound::Request.db` (multi-key op lane), and applied on the **owning**
  shard in `run_dispatch` / `run_dispatch_into` / the `Inbound::Request`
  handler / `blocked` wake / `block_xshard_target` serve,
- reset to 0 on internal entries (replication apply, ticks, persist).

`DbStores` implements `Deref`/`DerefMut` to the current db's `Store`, so all
~200 existing `self.store.…` call sites operate on the selected db unchanged.

### Commands

| Command | Behaviour |
|---|---|
| `SELECT i` | intercepted in `kevy-rt` (`exec_db_admin.rs`, same pattern as `CLIENT SETNAME`): validates `i < databases`, sets `conn.db`, `+OK`. Errors byte-identical to Redis: `ERR value is not an integer or out of range`, `ERR DB index is out of range` |
| `FLUSHDB [ASYNC]` | fanned out to **every shard** (`Inbound::DbAdmin::FlushDb(db)`) — each shard owns a slice of every db |
| `FLUSHALL [ASYNC]` | same fan-out (`DbAdmin::FlushAll`), one `flushdb` event per db |
| `SWAPDB a b` | same fan-out (`DbAdmin::SwapDb`), swaps keyspaces + db_index stamps on every shard |
| `DBSIZE` | no interception needed — routed to the selected db via the cursor |
| `MOVE key db` | routed to the key's owning shard, executed cross-db in `exec_move` (Phase 3); Redis-exact errors, TTL moves with the key |
| keyspace notifications | channels now carry the real db: `__keyevent@<db>__:<event>` |

The embedded engine (`kevy-embedded`) stays single-db; its `SELECT` arm
returns Redis's `ERR DB index is out of range`.

### Config

`[server] databases = N` (default **16**, matching Redis; `1` restores the
previous single-DB behaviour). Bounded to `1..=1024` at boot.

## Phase 1 scope (this PR) — in-memory only

- `databases` config, `SELECT`, `FLUSHDB`/`FLUSHALL`/`SWAPDB`, per-db
  `DBSIZE`, per-db notification channels, cross-shard routing of the db
  index, unit tests (`dbs.rs`) + updated `SELECT` tests.
- AOF and snapshots are multi-DB aware since Phase 2; replication since
  Phase 3 (boot gate removed). `main.rs::validate_databases` only bounds
  `databases` to `1..=1024` now.

## Phase 2 — persistence (implemented)

- **AOF**: multi-DB logs carry a `SELECT <db>` marker frame whenever the db
  changes between logged writes (the same shape Redis's AOF uses) — emitted
  in `Shard::log` from the per-shard `aof_last_db` cursor, and forced again
  after a rewrite swap (`on_aof_reopened`). Replay (`shard_run` /
  `uring_setup`) moves the `DbStores` cursor on `SELECT` frames
  (`select_marker`), so each db replays into its own keyspace. AOF rewrite
  (`dump_aof`) iterates dbs through the new `SnapshotSource` db hooks
  (`db_count` / `for_each_entry_in_db` / `row_seg_files_in_db` /
  `for_each_hash_ttl_in_db`) and writes one `SELECT` frame per db.
  Single-db (`databases = 1`) never emits markers — the log format is
  byte-identical to before. `FLUSHDB` / `FLUSHALL` / `SWAPDB` are intercepted
  before dispatch, so they are logged explicitly — on the origin shard (the
  client frame) and on every receiving shard of the fan-out (each shard owns
  its own AOF; a flush missing from one shard's log would resurrect keys on
  restart). Replay applies `SWAPDB` frames via `replay_admin_frame` (the verb
  has no embedded dispatch route); `SELECT` frames are never logged as
  commands — the marker is synthesized by the writer.
- **Snapshot**: one file per db per shard — db 0 keeps the historical
  `dump-<shard>.rdb` (format unchanged), db N > 0 gets
  `dump-<shard>-db<N>.rdb` (`layout::snapshot_db_path`). SAVE/BGSAVE freeze
  one COW view per db (`collect_snapshot_all`) and the persist worker writes
  each file; boot loads them per db (`load_boot_snapshot`), under the same
  `settle_snapshot` decision as db 0. The format itself is untouched, so
  single-db files remain readable and vice versa.
- **Row segments / tiering**: per-db directories — `segs-<shard>` for db 0
  (unchanged), `segs-<shard>-db<N>` for db N > 0 (`layout::segs_dir_db`), so
  per-db manifests never collide; tier vlogs were already
  `<tier root>/<shard>/db<db>`. Replay applies SEGMENTED frames against the
  current db's directory.
- **`INFO keyspace`**: one `dbN:keys=…,expires=…` line per non-empty db, as
  Redis does. Per-db gauges are published each reactor tick through the new
  `Commands::on_db_gauges` hook (`ShardStats::keys_db`/`expires_db`, sized
  from `databases` in `ObsState::with_databases`). The scalar `keys` gauge
  stays pinned to db0.
- Multi-DB as a replication replica is supported (Phase 3): per-db
  snapshot ship + `SELECT` marker frames in the backlog.

## Semantics notes (Redis-compatible)

- `SELECT` errors are byte-identical to Redis: `DB index is out of range`
  for a numeric index outside `0..databases`, `value is not an integer or
  out of range` otherwise.
- `FLUSHDB`/`FLUSHALL` accept `SYNC`/`ASYNC` and dirty every watched key of
  the flushed db(s), so a pending `EXEC` aborts.
- `SWAPDB a b` swaps the per-shard keyspaces but keeps each watch-version
  map attached to its logical db slot and dirties both dbs
  (`touchAllWatchedKeysForDb` on both, as Redis does) — an `EXEC` on
  either side aborts, and watch registrations are not moved across slots.
- `RESET` returns the connection to db 0.


## Phase 3 — replication, MOVE, cluster (implemented)

- **Replication backlog**: the feed carries the same `SELECT <db>` marker
  frames as the AOF (`Shard::push_feed`, cursor `feed_last_db`), so a replica
  replays each db into its own keyspace (`apply_replica_frame` applies the
  markers. The replica tracks its own apply cursor (`repl_apply_db`, starting
  at db 0 like the feed) separately from the snapshot ship, so a live frame
  without a marker lands in the right db after a per-db ship.
  `SWAPDB`/`MOVE`/`FLUSHDB` frames are pushed to the feed on the origin shard
  and on every fan-out shard, and replay via `replay_admin_frame` /
  embedded dispatch. `FLUSHALL` keeps the generation-bump resync contract
  (no flush frame on the feed) on origin and fan-out shards alike.
  The boot gate on the replica role is gone.
- **Snapshot ship**: the primary ships one snapshot stream per db — db 0 with
  the historical `+SNAPSHOT\r\n` marker, db N with `+SNAPSHOT <N>\r\n`
  (`encode_snapshot_begin_db`) — chained in `replication_pump`. The replica
  flushes everything on the db-0 ship, then loads each stream into its db
  (`SnapshotBegin { db }`). Empty dbs ship too, so the replica clears them.
- **`MOVE key db`**: routed to the key's owning shard (`Route::Single(1)`),
  intercepted in `run_dispatch` (`exec_move`) because it needs both dbs'
  stores; Redis-exact errors, TTL moves with the key, WATCH dirtied on both
  dbs, frame logged + pushed to the feed. Replay handles `MOVE` frames.
- **Cluster mode**: slots are computed over keys only; as Redis cluster is
  single-db by design, kevy cluster mode stays db-agnostic (multi-db is
  per-node, non-cluster).

## Non-goals

- AUTH/ACL/TLS remain out of scope by charter (separate topic).
- `SELECT` is intercepted at the reactor before MULTI queueing, so inside a
  transaction it applies immediately instead of being queued — a deliberate,
  documented deviation; the db switch applies to the commands that follow.

## Testing

- Unit: `kevy-rt/src/dbs.rs` (isolation, swap, flushall), `kevy/src/tests.rs`
  (SELECT errors byte-identical to Redis).
- Live (this branch, 11 shards): 500-key cross-shard isolation db0/db1,
  `redis://…/3` URL isolation, `SWAPDB`/`FLUSHDB`/`FLUSHALL` across shards —
  all verified against a running server.
