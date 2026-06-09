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

- Server: `BGREWRITEAOF`.
- Embedded: `Store::rewrite_aof() -> io::Result<Option<RewriteStats>>` (synchronous; blocks until the atomic rename completes).

**Automatic** (Redis-style thresholds): rewrite fires when the live AOF has
grown `percentage` past its size at the previous rewrite **and** is at least
`min_size` bytes. Defaults **100 % / 64 MiB**.

- Server: config keys `auto_aof_rewrite_percentage` / `auto_aof_rewrite_min_size` (also live-tunable via `CONFIG SET`); checked on the reactor tick.
- Embedded: `Config::with_auto_aof_rewrite(pct, min_size)`; checked on the background reaper tick, or on your `Store::tick` calls in manual reaper mode. `pct = 0` disables (manual only).

The embedded **auto**-rewrite is **non-blocking**: it serializes the keyspace
under the lock, then releases the lock for the slow disk write + fsync (the
expensive part), and re-takes it only briefly to swap the file in. Writes that
land during the disk write are tee'd into a diff buffer and appended after the
snapshot, so nothing is lost. The transient cost is one in-memory copy of the
serialized keyspace. The **manual** `Store::rewrite_aof()` / server
`BGREWRITEAOF` is synchronous (blocks the caller for the rewrite) — it's the
explicit "rewrite now" path. If a rewrite crashes midway the original AOF is
untouched (the swap is an atomic `rename`) and the `.tmp` file can be deleted.

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
| `<aof>.tmp` | In-progress rewrite/snapshot; safe to delete if stale |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | Corrupt AOF tail set aside during recovery |
