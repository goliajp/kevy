# Persistence

How kevy keeps data across restarts: the AOF, the binary snapshot, TTL
semantics, AOF rewrite/compaction, crash recovery, and the introspection you
can use to watch it all. Applies to both the network server (`kevy` binary)
and the in-process embedded mode (`kevy_embedded::Store`); differences are
called out.

## The two on-disk artifacts

Persistence lives in one directory (server: `--dir` / `KEVY_DIR`; embedded:
`Config::with_persist(dir)`). Each shard owns its own files, suffixed by shard
id:

| File | What | Written by |
|---|---|---|
| `aof-<id>.aof` | Append-only log of every mutating command (RESP frames, `KEVYAOF1`-magic-prefixed) | Continuously, as writes happen |
| `dump-<id>.rdb` | Binary point-in-time snapshot (`KEVYSNAP` magic) | Only on explicit `SAVE` / `BGSAVE` (server) or `Store::save_snapshot` (embedded) |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | A corrupt AOF tail moved aside during recovery | On startup, if the AOF tail can't be parsed |

A fresh embedded store with the default config persists via the **AOF only** —
no snapshot file appears unless you call `save_snapshot`. That is expected, not
a missing feature: the AOF alone is a complete durable record.

## Snapshots: `SAVE`, `BGSAVE`, `Store::save_snapshot`

A successful snapshot **resets the AOF**: the snapshot carries the state at
the snapshot point, and the log restarts with exactly the writes that landed
after it — so a restart loads snapshot + log without ever double-applying
history (v1.16.0 also fixed the embedded `save_snapshot`, which previously
left the full log in place and duplicated non-idempotent commands like
`RPUSH` on replay).

- **`SAVE`** (server) is synchronous and blocking-durable, the Redis
  contract: it returns after the snapshot is on disk. If a background
  save/rewrite is already in flight on a shard, that shard's `SAVE` is
  skipped with a log line.
- **`BGSAVE`** (server, v1.16.0) freezes a copy-on-write view per shard and
  returns immediately; a background thread writes the snapshot and the
  reactor tick commits the snapshot rename together with the log reset in
  one adjacent critical section. Writes issued after `BGSAVE` keep landing
  in the old log until the swap, so a crash at any point loses nothing.
- **`Store::save_snapshot`** (embedded) is synchronous like `SAVE`, with
  the same view-freeze trick: per-shard locks are held for the freeze and
  the commit, not the disk write.

## fsync policy (`appendfsync`)

Controls how often the AOF is flushed to disk. Default **`EverySec`** (matches
Redis).

| Policy | Durability | Cost |
|---|---|---|
| `Always` | Zero-loss (every write fsynced before its reply) | ~50 % throughput |
| `EverySec` (default) | ≤ 1 s of writes lost on a crash | Cheap |
| `No` | OS pagecache decides | Cheapest |

Set it server-side with the `appendfsync` config key (`always` / `everysec` /
`no` in the TOML file, also live-tunable via `CONFIG SET appendfsync …`), and
embedded with `Config::with_appendfsync(AppendFsync::Always)`.

In embedded mode the `EverySec` flush window is driven off the background TTL
reaper tick (or your `Store::tick` calls in manual reaper mode).

## TTL persistence — absolute deadlines (v1.8.1+)

TTLs are persisted as **absolute Unix-millisecond deadlines** (`PEXPIREAT` in
the AOF, an absolute field in snapshot format v3). A key keeps its original
expiry instant across any number of restarts; time the process spent down is
correctly subtracted, and a key whose deadline already passed is dropped on
load.

> Before v1.8.1 TTLs were stored relative (`PEXPIRE <remaining>`) and
> re-anchored to load-time, so every restart reset a key to a fresh full TTL.
> Fixed in v1.8.1 (INC-2026-06-09). Old relative-`PEXPIRE` AOF entries and v2
> snapshots still load (treated as relative) — no migration needed; new writes
> are absolute. `EXPIREAT` / `PEXPIREAT` are also exposed as client commands.

## AOF rewrite / compaction

The AOF grows with every write, including repeated overwrites of the same key.
**Rewrite** rebuilds it as the minimal command set that reconstructs the
current keyspace (one `SET`/`HSET`/… per key, plus a `PEXPIREAT` for TTL'd
keys), then atomically replaces the live file — so 10 000 `SET hot …` collapse
to a single `SET hot <latest>`.

**Manual** (always available):

- Server: `BGREWRITEAOF` — background since v1.16.0: `+OK` returns once the
  shard has frozen a copy-on-write view of its keyspace (O(n)-shallow,
  nanoseconds per key); a per-shard background thread serializes it and the
  compacted file swaps in within a reactor tick (~100 ms) of the disk write
  finishing. Watch `INFO persistence` (below) for completion. One job in
  flight per shard; a request landing while one runs is skipped with a log
  line.
- Embedded: `Store::rewrite_aof() -> io::Result<Option<RewriteStats>>` —
  synchronous from the caller's point of view (returns after the atomic
  rename), but per-shard locks are held only for the view freeze and the
  final swap; concurrent readers/writers flow during the serialization.

**Automatic** (Redis-style thresholds): rewrite fires when the live AOF has
grown `percentage` past its size at the previous rewrite **and** is at least
`min_size` bytes. Defaults **100 % / 64 MiB**.

- Server: config keys `auto_aof_rewrite_percentage` / `auto_aof_rewrite_min_size` (also live-tunable via `CONFIG SET`); checked on the reactor tick.
- Embedded: `Config::with_auto_aof_rewrite(pct, min_size)`; checked on the background reaper tick, or on your `Store::tick` calls in manual reaper mode. `pct = 0` disables (manual only).

Every rewrite path is **non-blocking** for the keyspace (v1.16.0): the lock
(embedded) or shard thread (server) is held only to freeze a copy-on-write
view — collection values are refcount-shared, so the freeze is O(keys), not
O(bytes) — and to swap the finished file in. Serialization + fsync run with
the keyspace live; writes that land meanwhile are tee'd into a diff buffer
and appended after the compacted image, so nothing is lost. The transient
cost is the view (tens of bytes per key) plus a one-time copy of any
collection first mutated while the rewrite is in flight. If a rewrite
crashes midway the original AOF is untouched (the swap is an atomic
`rename`) and the `<aof>.rewrite` temp can be deleted.

## Crash recovery (AOF replay on startup)

On open, kevy loads `dump-<id>.rdb` if present, then replays `aof-<id>.aof`:

- **Clean** → all commands applied.
- **Truncated tail** (crash mid-append) → the prefix replays; the partial
  trailing frame is dropped. Recoverable, no data loss beyond the unfinished
  write.
- **Corrupt frame** (e.g. non-kevy bytes written into the file path) → the
  prefix replays, the bad tail is dropped, and the offending bytes are moved to
  `aof-<id>.aof.panic-quarantine.<unix_ts>` so they don't block future starts.
  The quarantined tail is *not* re-applied; inspect it manually if you need to
  recover anything from it.

Each replay logs a one-line summary including wall-clock time:

```
kevy: AOF /data/kevy/aof-0.aof replayed 145313 commands from 418261733 bytes in 247 ms (clean)
```

Because the AOF is unbounded, replay time grows with it — watch this number
and use auto-rewrite to keep it bounded.

A shard-layout migration (changing `--threads` / `shards`, or switching
cluster routing) is crash-idempotent: new snapshots land under `.reshard`
temp names, a durable `reshard.journal` marks the commit point, and an
interrupted migration is rolled forward on the next start. Source files
survive as `.premigration.<timestamp>` backups.

## Introspection (server)

`INFO persistence` reports the answering shard's view, refreshed each
reactor tick (~100 ms):

```
aof_enabled:1
appendfsync:everysec
aof_rewrite_in_progress:0     # a background save/rewrite is in flight
aof_rewrites_total:3          # completed AOF rewrites since open
```

`aof_rewrites_total` incrementing (and `in_progress` returning to `0`) is
the completion signal for the asynchronous `BGREWRITEAOF` / `BGSAVE`.

## Introspection (embedded)

In-process mode has no TCP endpoint for `redis-cli`, so the same signals are
methods on the `Store` handle:

```rust
store.dbsize();                 // live key count
store.info();                   // KevyInfo { keys, used_memory, aof_bytes,
                                //            expire_pending, evictions, expired_keys }
store.ttl(key);                 // Option<Duration> (None = no key / no TTL)
store.ttl_ms(key);              // raw Redis PTTL: -2 no key, -1 no TTL, else ms
store.expire_pending_count();   // live keys carrying a TTL (expire-set size)
store.used_memory();            // resident-bytes estimate
store.expired_keys_total();     // total expired (lazy + reaper)
store.evictions_total();        // total evicted by maxmemory
```

`expire_pending_count() == 0` when you expected TTLs is the tell that the TTL
subsystem didn't register your keys.

### Push-style metrics

For continuous monitoring (vs polling `info()`), register a callback:

```rust
let cfg = Config::default()
    .with_persist("/data/kevy")
    .with_metric_sink(|m| match m {
        KevyMetric::Replay { commands, bytes, elapsed_ms } => { /* startup */ }
        KevyMetric::Rewrite { keys, before_bytes, after_bytes, elapsed_ms } => { /* compaction */ }
        _ => {}
    });
```

The sink fires on AOF replay (startup) and each AOF rewrite (compaction). It
runs synchronously on the emitting thread (the reaper thread for background
rewrites), so keep it fast. `KevyMetric` is `#[non_exhaustive]` — match with a
`_` arm to stay forward-compatible.

## What is *not* persisted

- **Pub/sub** — channels, subscriptions, and published messages are in-memory
  only; nothing about pub/sub is written to the AOF or snapshot.
- **Blocking-command waiters** (`BLPOP`, blocking `XREAD`) — connection state,
  not data.

## File-naming reference

| Pattern | Meaning |
|---|---|
| `aof-<id>.aof` | Live AOF for shard `<id>` |
| `dump-<id>.rdb` | Binary snapshot for shard `<id>` |
| `shards.meta` | Recorded shard count + routing scheme |
| `dump-<id>.rdb.tmp` | In-progress snapshot write; safe to delete if stale |
| `aof-<id>.aof.rewrite` | In-progress AOF rewrite/reset; safe to delete if stale |
| `dump-<id>.rdb.reshard` + `reshard.journal` | In-progress shard-layout migration (rolled forward on next start; never delete the journal by hand) |
| `*.premigration.<unix_ts>` | Pre-migration source backups |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | Corrupt AOF tail set aside during recovery |
