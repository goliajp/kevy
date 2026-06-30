# Persistence

How kevy keeps data across restarts — the AOF, snapshots, fsync policies, rewrite/compaction, crash recovery, and the introspection that lets you watch all of it.

## When you need this

Reach for this doc when you are:

- Choosing a durability policy (zero-loss vs throughput) for a production deployment.
- Sizing disk usage and replay-time budgets for a write-heavy workload.
- Debugging an unexpected on-disk artifact — a quarantine file, a stale `.rewrite` temp, a `.premigration.*` backup.
- Wiring an embedded `kevy_embedded::Store` into a host application and want to know what survives a process crash, what doesn't, and how to observe it from inside the host.
- Looking at a key whose TTL behaves oddly across restarts.

If you only want a quick "does it survive `kill -9`?" answer: yes, with at most one second of writes lost under the default policy.

## Core idea

Every shard owns two files in the persistence directory: an append-only log of mutating commands (`aof-<id>.aof`) and an optional binary snapshot (`dump-<id>.rdb`). The AOF alone is a complete durable record; the snapshot exists only to bound replay time. On boot kevy loads the snapshot if present, then replays the AOF; on a successful snapshot the AOF is reset so the two files together cover the full history exactly once.

## Worked examples

### Server mode

Drop this into `kevy.toml` and launch with `kevy --config kevy.toml`:

```toml
# kevy.toml
dir         = "/var/lib/kevy"
port        = 6379
threads     = 4
appendonly  = true

# AOF durability — see the knobs table below for the full set.
appendfsync                 = "everysec"   # always | everysec | no
auto_aof_rewrite_percentage = 100          # rewrite when AOF doubles since last rewrite
auto_aof_rewrite_min_size   = 67108864     # …and is at least 64 MiB
```

Operate it with standard Redis-style commands over RESP:

```text
$ redis-cli -p 6379 BGSAVE
Background saving started

$ redis-cli -p 6379 BGREWRITEAOF
Background append only file rewriting started

$ redis-cli -p 6379 INFO persistence
aof_enabled:1
appendfsync:everysec
aof_rewrite_in_progress:0
aof_rewrites_total:3
```

`CONFIG SET appendfsync always` retunes the policy live without a restart.

### Embedded mode

Add the crate to `Cargo.toml`:

```toml
[dependencies]
kevy-embedded = "*"
```

Then in `main.rs`:

```rust
use std::time::Duration;
use kevy_embedded::{AppendFsync, Config, KevyMetric, Store};

fn main() -> std::io::Result<()> {
    let cfg = Config::default()
        .with_persist("/var/lib/myapp/kevy")
        .with_appendfsync(AppendFsync::EverySec)
        .with_auto_aof_rewrite(100, 64 * 1024 * 1024)
        .with_metric_sink(|m| match m {
            KevyMetric::Replay { commands, bytes, elapsed_ms } => {
                eprintln!("kevy replay: {commands} cmds / {bytes} B in {elapsed_ms} ms");
            }
            KevyMetric::Rewrite { keys, before_bytes, after_bytes, elapsed_ms } => {
                eprintln!(
                    "kevy rewrite: {keys} keys, {before_bytes} -> {after_bytes} B in {elapsed_ms} ms"
                );
            }
            _ => {}
        });

    let store = Store::open(cfg)?;

    store.set("hello", b"world")?;
    store.pexpire("hello", Duration::from_secs(300))?;

    // Point-in-time snapshot. Returns after the file is on disk; per-shard
    // locks are held only for the view freeze and the final rename.
    store.save_snapshot()?;

    // On-demand AOF compaction. Same lock discipline as save_snapshot.
    let _stats = store.rewrite_aof()?;

    // Live introspection.
    let info = store.info();
    println!("{} keys, {} bytes AOF", info.keys, info.aof_bytes);

    Ok(())
}
```

A fresh embedded store with the default config writes only the AOF — no snapshot file appears until `save_snapshot` runs. That is expected; the AOF on its own is enough to rebuild the keyspace.

## Configuration knobs

### Durability and AOF growth

| Knob | Server (TOML / `CONFIG SET`) | Embedded (`Config::…`) | Default | Notes |
|---|---|---|---|---|
| AOF fsync policy | `appendfsync` (`always` / `everysec` / `no`) | `with_appendfsync(AppendFsync::…)` | `EverySec` | Live-tunable on the server. |
| AOF enabled | `appendonly` (`true` / `false`) | implied by `with_persist(...)` | `true` (server), off until `with_persist` | Disabling skips all on-disk persistence. |
| Auto-rewrite percentage | `auto_aof_rewrite_percentage` | first arg of `with_auto_aof_rewrite(pct, min)` | `100` | `0` disables auto-rewrite. |
| Auto-rewrite minimum size | `auto_aof_rewrite_min_size` | second arg of `with_auto_aof_rewrite(pct, min)` | `67108864` (64 MiB) | Auto-rewrite fires only when both thresholds are met. |
| Persistence directory | `dir` / env `KEVY_DIR` | `with_persist(path)` | `./data` (server); none (embedded) | One directory per kevy instance. |
| Reactor / reaper cadence | reactor tick, ~100 ms | background reaper, or your `Store::tick` calls | ~100 ms | Drives `EverySec` flush, auto-rewrite checks, TTL eviction. |

### Trigger surface

| Action | Server | Embedded | Blocking shape |
|---|---|---|---|
| Synchronous snapshot | `SAVE` | `Store::save_snapshot()` | Returns after the file is on disk; locks held only for freeze + rename. |
| Background snapshot | `BGSAVE` | call `save_snapshot` from a worker thread | Returns immediately; commit lands within one reactor tick of the disk write finishing. |
| AOF rewrite | `BGREWRITEAOF` | `Store::rewrite_aof()` | Returns after the atomic rename; serialization runs with the keyspace live. |
| Live-tune fsync | `CONFIG SET appendfsync everysec` | rebuild `Config` | n/a |

### fsync policy semantics

| Policy | Durability | Cost |
|---|---|---|
| `Always` | Zero-loss — every write fsynced before its reply | ~50% throughput |
| `EverySec` (default) | At most ~1 second of writes lost on a crash | Cheap |
| `No` | Defers to the OS pagecache flush | Cheapest |

## Trade-offs and limits

**Per-policy throughput vs data loss.** `Always` blocks each reply on `fsync`; it is the only policy that survives `kill -9` with zero command loss, and it cuts SET-heavy throughput roughly in half on typical NVMe. `EverySec` runs a background flush every second and loses up to that window on a crash — the default precisely because it matches the Redis trade and the lost window is usually tolerable. `No` lets the kernel decide; throughput is highest but a crash can lose anything still in pagecache, potentially many seconds.

**AOF replay cost vs snapshot load cost.** Without a snapshot, boot time grows linearly with the AOF byte count: a 4 GiB AOF replays in a few seconds on local NVMe, a 40 GiB one in a minute or more. A snapshot caps that — load is one streaming read plus a short tail of post-snapshot AOF — but costs a transient view freeze (O(keys), nanoseconds per key, because collection values are refcount-shared) plus a one-time copy of any collection first mutated while the snapshot is in flight. For write-heavy workloads, prefer leaning on auto-rewrite to keep the AOF bounded rather than running periodic `BGSAVE`s: rewrite gives you the same boot-time bound with no second file to manage.

**Background-job concurrency.** Each shard runs at most one background save or rewrite at a time. A duplicate request that arrives mid-job is skipped with a log line, never queued.

**TTL persistence.** TTLs are written as absolute Unix-millisecond deadlines (`PEXPIREAT` in the AOF, an absolute field in the snapshot format), so a key keeps its original expiry instant across any number of restarts and the time the process spent down is subtracted correctly. Older AOFs that recorded relative remaining time still load (treated as relative on entry); new writes are always absolute. `EXPIREAT` and `PEXPIREAT` are exposed as client commands.

**Shard-layout changes are crash-idempotent.** Changing `--threads` / `shards` writes new snapshots under `.reshard` temp names, commits via a durable `reshard.journal`, and rolls an interrupted migration forward on the next start. Source files survive as `.premigration.<unix_ts>` backups; the journal is the commit point and must never be deleted by hand.

**What is not persisted.** Pub/sub channels, subscriptions, and undelivered messages live only in memory. Blocking-command waiters such as `BLPOP` and blocking `XREAD` are connection state, not data. Neither is written to the AOF or snapshot, and neither is replayed.

## FAQ

### My AOF file is growing — how do I compact it?

Run `BGREWRITEAOF` on the server or `Store::rewrite_aof()` in embedded mode. Rewrite rebuilds the log as the minimal command set that reconstructs the current keyspace — one `SET` / `HSET` / etc. per key, plus a `PEXPIREAT` for TTL'd keys — and atomically swaps the new file in. Ten thousand overwrites of `hot` collapse to a single `SET hot <latest>`.

For unattended ops, leave auto-rewrite at its defaults — 100% growth over the previous rewrite size, with a 64 MiB floor — and the reactor will fire compaction on its own. Set `auto_aof_rewrite_percentage = 0` to disable it and drive rewrite entirely by hand.

Rewrite is non-blocking for the keyspace: serialization and `fsync` run with reads and writes flowing, and any writes that land during the rewrite are tee'd into a diff buffer that gets appended to the compacted image. If a rewrite crashes midway the original AOF is untouched (the swap is an atomic `rename`) and the leftover `aof-<id>.aof.rewrite` temp is safe to delete.

### Can I disable persistence entirely?

Yes, in two ways:

- **Server:** set `appendonly = false` in `kevy.toml` (or omit `--dir`). The server runs as a pure in-memory cache; no `aof-*` or `dump-*` files are created.
- **Embedded:** build a `Config` without calling `with_persist(...)`. `Store::open` runs the keyspace entirely in memory; `save_snapshot` and `rewrite_aof` become no-ops at the API surface (or surface an error indicating no persistence directory is configured).

If you want persistence but no AOF growth at all between snapshots, that combination is not supported — kevy's durability model is AOF-first, and the snapshot exists to bound AOF replay, not to replace the AOF.

### What is the cost of a snapshot during high write load?

The blocking portion is tiny. A per-shard freeze of the keyspace is O(keys), not O(bytes), because collection values are reference-counted and shared with the live store; on a million-key shard the freeze takes single-digit milliseconds. Serialization itself runs with the keyspace live — writes are not paused.

The transient cost you pay is memory. Any collection (list, hash, set, sorted-set) that gets mutated while the snapshot is being written gets cloned once, so the live store can move on without disturbing the frozen view. For workloads dominated by `SET` on plain string keys the extra memory is negligible; for workloads dominated by `HSET` / `LPUSH` on a small number of huge collections it can briefly double the resident size of those specific collections.

A successful snapshot also resets the AOF — the snapshot now carries everything the log used to, and the log restarts with only writes that landed after the freeze. A restart then loads snapshot + log without ever double-applying history.

### How is recovery sequenced on the next boot?

For each shard, in order:

1. **Load the snapshot.** If `dump-<id>.rdb` exists, stream it into the keyspace. Expired TTLs are dropped during load.
2. **Replay the AOF.** Read `aof-<id>.aof` from the front and apply each frame.
3. **Handle the tail.** A clean file applies in full. A truncated tail (crash mid-append) drops the partial trailing frame and applies the prefix. A corrupt frame moves the bad bytes aside to `aof-<id>.aof.panic-quarantine.<unix_ts>` so they don't block future starts, then applies the prefix. The quarantined tail is never re-applied; inspect it by hand if you need to recover anything from it.
4. **Log a one-line summary** including wall-clock time:

   ```text
   kevy: AOF /data/kevy/aof-0.aof replayed 145313 commands from 418261733 bytes in 247 ms (clean)
   ```

5. **Roll forward any interrupted shard-layout migration** by replaying `reshard.journal`.

Watch the replay-time line and use auto-rewrite to keep it bounded — replay time grows linearly with the unrewritten AOF size.

### How do I monitor persistence from inside an embedded host process?

Two surfaces.

**Polling.** `store.info()` returns a `KevyInfo` struct with `keys`, `used_memory`, `aof_bytes`, `expire_pending`, `evictions`, `expired_keys`. Finer-grained helpers cover the same ground:

```rust
store.dbsize();                 // live key count
store.ttl(key);                 // Option<Duration> (None = no key / no TTL)
store.ttl_ms(key);              // Redis PTTL semantics: -2 no key, -1 no TTL, else ms
store.expire_pending_count();   // live keys carrying a TTL
store.used_memory();            // resident-bytes estimate
store.expired_keys_total();     // total expired (lazy + reaper)
store.evictions_total();        // total evicted by maxmemory
```

`expire_pending_count() == 0` when you expected TTLs is the classic tell that the TTL subsystem didn't register your keys.

**Push.** Register `Config::with_metric_sink(...)` and receive `KevyMetric` events on AOF replay (startup) and each AOF rewrite (compaction). The sink runs synchronously on the emitting thread (the reaper for background rewrites), so keep the callback fast. `KevyMetric` is `#[non_exhaustive]` — always match a `_` arm to stay forward-compatible.

### What is every file in the persistence directory?

| Pattern | Meaning |
|---|---|
| `aof-<id>.aof` | Live AOF for shard `<id>`. |
| `dump-<id>.rdb` | Binary snapshot for shard `<id>`. |
| `shards.meta` | Recorded shard count and routing scheme. |
| `dump-<id>.rdb.tmp` | In-progress snapshot write. Safe to delete if stale. |
| `aof-<id>.aof.rewrite` | In-progress AOF rewrite/reset. Safe to delete if stale. |
| `dump-<id>.rdb.reshard` + `reshard.journal` | In-progress shard-layout migration. Rolled forward on next start; never delete the journal by hand. |
| `*.premigration.<unix_ts>` | Pre-migration source backups, kept for rollback. |
| `aof-<id>.aof.panic-quarantine.<unix_ts>` | Corrupt AOF tail set aside during recovery. Inspect by hand if you need to salvage anything; kevy will not re-apply it. |
