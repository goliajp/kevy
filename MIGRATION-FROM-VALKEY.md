# Migrating from valkey / redis to kevy

kevy speaks RESP2 — the same wire protocol valkey and Redis speak. Every
RESP2 client (redis-rs, go-redis, jedis, lettuce, node-redis, ioredis,
phpredis, redis-py, hiredis, …) connects without code changes.

**TL;DR:** point your client at kevy's port (default `6004`), keep using
the same commands. kevy implements the valkey/redis command set across
all 5 data types plus Streams, Geo, blocking pops (`BLPOP` / `BRPOP` /
`XREAD BLOCK`), pub/sub (channels + patterns), and transactions (with
optional `WATCH` CAS), parity-verified vs valkey 9.1 + redis 7.4.

This guide tracks **workspace v1.5.0** (current) — verify your binary
with `kevy --version` if anything in this doc looks off.

## What's the same as valkey 9.1

| | |
|---|---|
| Wire protocol | RESP2 (full) |
| Reply shapes | bulk / simple-string / integer / array / error / null-bulk — identical encoding |
| Error codes | `WRONGTYPE`, `ERR wrong number of arguments`, etc. — kept verbatim |
| Command semantics | reply-by-reply identical (see the parity table below) |
| Subscription model | `SUBSCRIBE` / `UNSUBSCRIBE` / `PUBLISH` — channel-based; `PSUBSCRIBE` / `PUNSUBSCRIBE` — Redis-glob patterns (`*`, `?`, `[…]`) |
| Transactions | `MULTI` / `EXEC` / `DISCARD` — queue + atomic execute; `WATCH` / `UNWATCH` optimistic CAS (single-shard strict, cross-shard best-effort — same property as Redis cluster mode) |

## What's different

Most of these are **intentional scope choices** (detailed below); a few are roadmap items in flight.

### Permanently out of scope

- **Cluster mode.** kevy is single-machine by design. For sharding
  across hosts, front N kevy instances with twemproxy / envoy
  cluster-routing. `CLUSTER INFO` returns `cluster_enabled:0` so
  cluster-aware clients fall back to single-node mode.
- **Replication.** Single-machine; HA path is k8s StatefulSet +
  persistent volume, not in-kevy primary→replica.
- **Lua / Functions scripting** (`EVAL`, `EVALSHA`, `FUNCTION`).
  Roadmap candidate for v1.x; not in v1.0.

### Out of scope (permanent)

- **AUTH / TLS** (`AUTH`, `requirepass`, `tls-port`). kevy's target
  scenarios (docker-compose internal, k8s pod network, embedded,
  cache) put the trust boundary at the network layer, so auth and TLS
  are intentionally not on the roadmap — same posture as cluster mode
  and replication. kevy WARNs at startup if you bind to a non-loopback
  interface.

### Still deferred

- Incremental `SCAN` cursor (currently a single full pass; cursor `0`
  is the canonical entry point — replies are valkey-shaped)
- `EVAL` / `EVALSHA` / `FUNCTION` (Lua scripting) — needs a pure-Rust
  Lua engine to keep the 0-deps charter; no committed timeline
- Non-blocking multi-stream `XREAD` across shards currently reads only
  the first STREAMS key's shard (blocking `XREAD` is fully cross-shard)

### Landed since v1.0 (previously deferred)

- `RENAME` / `RENAMENX` across shards — two-phase take→put orchestrator
  (same-shard stays one atomic op).
- True RESP3 — `HELLO 3` negotiates proto 3; replies use RESP3 shapes
  (Map / Set / Double / Big-number / Null) per command.
- **Streams** — `XADD` / `XREAD` / `XRANGE` / `XREVRANGE` / `XLEN` /
  `XDEL` / `XTRIM` plus consumer groups (`XGROUP` / `XREADGROUP` /
  `XACK` / `XPENDING` / `XCLAIM` / `XAUTOCLAIM` / `XINFO`).
- **Blocking pops** — `BLPOP` / `BRPOP` / `XREAD BLOCK` / `XREADGROUP
  BLOCK`, single- and multi-key, **across shards** (v1.5.0).
- Keyspace notifications — `__keyspace@*__:*` / `__keyevent@*__:*`.
- **Geo** — `GEOADD` / `GEOSEARCH` / `GEOSEARCHSTORE` / `GEODIST` /
  `GEOPOS` / `GEOHASH`.
- `SLOWLOG` — `GET` / `LEN` / `RESET` (per-shard ring, merged on read).
- `WATCH` / `UNWATCH` — optimistic CAS for transactions. Single-shard
  is strict CAS (reactor is single-threaded per shard); cross-shard is
  best-effort with a µs-scale window between the pre-EXEC `CheckWatch`
  fan-out and the queued commands running — the same trade-off Redis
  cluster mode has. EXEC returns `Nil` on a watched-key conflict.
- `PSUBSCRIBE` / `PUNSUBSCRIBE` — Redis-glob pattern pub/sub. Patterns
  use `*` / `?` / `[…]` (with `^` negation + `\` escape). `PUBLISH`
  delivers a `message` frame per direct subscriber AND a `pmessage`
  frame per matching pattern subscriber, with reply count summing
  both. Pattern matching adds one `HashMap::is_empty()` check per
  delivery — zero hot-path cost when no pattern subscribers exist
  (the common case).

### Different internals (transparent to clients)

- Hash table: open-addressing Swiss table + SIMD group scan, not
  Redis's listpack + hashtable. **Same `O(1)` semantics**, different
  memory footprint.
- Sorted set: `HashMap<member, score>` + `BTreeSet<(score, member)>`,
  not Redis's skiplist. Same `O(log n)` rank semantics; `ZRANK` is
  currently `O(n)` (order-statistics tree is a polish item).
- List: `VecDeque` (ring buffer), not Redis's quicklist. `O(1)` ends,
  `O(n)` middle splice.
- Set: `HashSet`, not Redis's listpack-or-hashtable encoding.
- String: `SmallBytes` with inline SSO for ≤ 22-byte values.

These are **behaviour-compatible** ("Hash works like a Hash") but
**not encoding-compatible** — kevy snapshot files and AOFs are not
interoperable with Redis/valkey. (That's a non-goal.)

## Command parity (94 checks via bench/compat3.sh)

`bench/compat3.sh` runs the same command sequence against valkey 9.1,
redis 7.4, and kevy via the neutral `valkey-cli`, then diffs replies
byte-for-byte. Last run: **94 / 94 pass**, 0 mismatches.

The 94 checks below are the cross-engine *differential* core (Strings /
Keys / Hash / List / Set / Sorted set / pub/sub / transactions /
persistence). Streams, Geo, and blocking pops (listed under "Landed"
above) ship with their own Rust integration suites
(`crates/kevy/tests/`) rather than this `valkey-cli` diff harness.

### Connection / admin (5)

```
PING ECHO HELLO QUIT COMMAND
```

### Operations (7)

```
INFO CLUSTER DEBUG WAIT SHUTDOWN CONFIG CLIENT
```

- `CONFIG GET <pattern>` — works (supports glob patterns, multi-arg
  query).
- `CONFIG SET` / `CONFIG REWRITE` — real hot-modification (since
  workspace v1.2.0). Read-only fields (bind, port, dir, etc.) still
  reply `ERR ... read-only — edit kevy.toml and restart`; everything
  else (`appendfsync`, `auto_aof_rewrite_percentage`, `maxmemory`,
  `maxmemory-policy`, `hz`, `loglevel`, …) applies to all shards
  within the next tick (default 100 ms). `CONFIG REWRITE` writes the
  effective config back to disk.
- `CLIENT GETNAME` / `SETNAME` / `ID` / `NO-EVICT` / `LIST` / `KILL` —
  reply shapes match Redis so client libraries that probe `CLIENT` at
  handshake (lettuce, ioredis, …) keep working. Per-connection state
  tracking is a stub; v1.x will wire it through to the reactor.

### Keys (12)

```
DEL EXISTS EXPIRE PEXPIRE TTL PTTL PERSIST TYPE
DBSIZE FLUSHDB FLUSHALL KEYS SCAN RANDOMKEY
```

### String (16)

```
SET GET MSET MGET GETSET GETDEL SETNX SETEX PSETEX
APPEND STRLEN INCR DECR INCRBY DECRBY INCRBYFLOAT
```

`SET` supports `EX` / `PX` / `NX` / `XX` modifiers.

### Hash (11)

```
HSET HSETNX HGET HDEL HEXISTS HLEN HINCRBY
HKEYS HVALS HGETALL HMGET
```

### List (10)

```
LPUSH RPUSH LPOP RPOP LLEN LINDEX LRANGE LSET LREM LTRIM
```

### Set (10)

```
SADD SREM SCARD SISMEMBER SMEMBERS SPOP SRANDMEMBER
SINTER SUNION SDIFF
```

### Sorted set (9)

```
ZADD ZSCORE ZCARD ZREM ZRANK ZINCRBY
ZRANGE ZRANGEBYSCORE ZCOUNT
```

`ZRANGEBYSCORE` supports `(min` / `min)` / `-inf` / `+inf` bounds.

### Pub/sub (5)

```
SUBSCRIBE UNSUBSCRIBE PSUBSCRIBE PUNSUBSCRIBE PUBLISH
```

`PSUBSCRIBE` patterns use Redis glob syntax (`*`, `?`, `[…]`, `^`
negation, `\` escape). Pattern subscribers receive `pmessage` frames;
direct + pattern subscribers both contribute to `PUBLISH`'s reply
count.

### Transactions (5)

```
MULTI EXEC DISCARD WATCH UNWATCH
```

`WATCH key [key ...]` is **strict CAS within one shard** (the
reactor is single-threaded per shard) and **best-effort across
shards** — exactly the trade-off Redis cluster mode has, for the
same reason (no global no-interleave lock). EXEC returns `Nil`
on a watched-key conflict; otherwise it commits the queued cmds.

### Persistence (3)

```
SAVE BGSAVE BGREWRITEAOF
```

## Persistence model

| | valkey / Redis | kevy v1.3 |
|---|---|---|
| Snapshot | RDB binary format | kevy snapshot v2 (own `KEVYSNAP` header, type-tagged) |
| AOF | append-only commands | append-only commands, `KEVYAOF1\n` magic header on fresh files (since v1.2.0) |
| AOF rewrite | `BGREWRITEAOF` (background fork) | `BGREWRITEAOF` (synchronous per shard in v1.x; incrementalisation is a v2 polish item) |
| Auto-rewrite | `auto_aof_rewrite_percentage` / `auto_aof_rewrite_min_size` | same knobs, same semantics (defaults: `100` / `64mb`) — exercised by `crates/kevy/tests/persistence.rs::auto_aof_rewrite_*` |
| fsync policy | `always` / `everysec` (default) / `no` | identical names + semantics |
| Legacy AOF replay | n/a | bare-RESP AOFs (pre-v1.2 files without the magic header) still replay cleanly — backward-compat verified on every release |
| Snapshot interoperable with Redis RDB? | yes | **no** (different format; migration is via `RESTORE` or app-level export) |

### Data-loss guarantees on crash

| `appendfsync` | Guarantee | Throughput cost |
|---|---|---|
| `always` | **0 bytes** lost — every write is on disk before `+OK` returns | ~50 % vs `everysec` |
| `everysec` (default) | ≤ **1 second** of writes lost (matches Redis) | baseline |
| `no` | up to ~30 s (kernel pagecache flush window) | slightly faster than `everysec` |

### Crash-safety contract

On startup each shard loads its snapshot (`dump-<id>.rdb`) first, then
replays its append-only log (`aof-<id>.aof`). The AOF parser tolerates
a truncated trailing frame from a process kill / power loss — the clean
prefix replays and the partial tail is silently dropped, never a startup
failure (verified by `crates/kevy/tests/persistence.rs`'s
`aof_truncated_tail_is_tolerated_on_restart`).

For destructive integration testing, `bench/crash-test.sh` loops
"start → SET 100 keys → kill -9 → restart → verify". Run as
`bash bench/crash-test.sh 10 everysec` (10 rounds with the default
fsync policy) or `… always` (zero-loss mode).

## Eviction policies

kevy ships all 8 Redis policies with identical names and identical
selection algorithms:

```
noeviction  (default)
allkeys-lru   allkeys-lfu   allkeys-random
volatile-lru  volatile-lfu  volatile-random  volatile-ttl
```

LRU/LFU approximation uses Redis-style 24-bit clock + sample-based
selection (configurable via `maxmemory-samples`, default 5).

Memory pressure is enforced before every write when `maxmemory` is
set; the check compiles out (dead-code-eliminated) when the knob
remains at its `0` (unlimited) default, so the eviction code path
costs zero on workloads that don't use it.

## Migration walkthrough

### 1. Spin up kevy in place of valkey

**docker-compose:**

```yaml
services:
  kv:
    image: golia/kevy:1.3       # (or build from source until image lands)
    environment:
      KEVY_BIND: 0.0.0.0        # trust-bounded inside the network
    ports:
      - "6004:6004"
    volumes:
      - kevy-data:/var/lib/kevy
```

**Direct binary:**

```sh
cargo build --release -p kevy
./target/release/kevy --port 6379  # same port as Redis default
```

### 2. Point your client

Either change the port to `6004` or run kevy on `6379`:

```python
import redis
r = redis.Redis(host="kv", port=6004)
r.set("foo", "bar")   # works
r.get("foo")          # b'bar'
```

### 3. (Optional) Drop the config

```toml
# kevy.toml
[server]
bind     = "0.0.0.0"
port     = 6379
threads  = 0
data_dir = "/var/lib/kevy"

[memory]
maxmemory        = "2gb"
maxmemory_policy = "allkeys-lru"

[persistence]
appendfsync = "everysec"
```

```sh
kevy --config /etc/kevy/kevy.toml
```

### 4. Verify with your test suite

If you have a Redis-targeted test suite, it should pass against kevy
unmodified for the 94 commands above. If a test fails, file an issue
with the reply diff — we treat parity gaps as bugs.

## When NOT to migrate

- You need **AUTH / TLS** today (use valkey 9.1 + `requirepass` and
  wait for kevy v0.3+)
- You need **replication** today (use valkey 9.1 + `replicaof`, or
  the k8s StatefulSet pattern with multiple independent kevy
  instances)
- You need **cluster** today (use valkey-cluster — kevy is permanently
  single-machine)
- You need **Lua scripting** today (use valkey 9.1 — kevy adds
  scripting in v1.x)
- You need **Streams / consumer groups** today (valkey 9.1 — kevy
  adds in v1.x)
- You need **Redis-RDB-format snapshot compatibility** (kevy uses its
  own snapshot format — won't change)

Otherwise: kevy is a drop-in faster replacement.

## Reporting compat gaps

Open an issue with:

```
1. The exact command (`redis-cli` syntax)
2. The valkey reply
3. The kevy reply
4. Why the diff matters for your use case
```

We treat reply mismatches as bugs unless the command is on the
"out of scope" list above.

## v1.x stability commitment

Everything below is contract — kevy promises to keep it for the entire
v1.x line. Breaking any of these requires a v2.0 major bump.

| Surface | Stability promise |
|---|---|
| **Persistence format** | AOF schema (RESP multi-bulk frames) v1.x-compatible; snapshot format `KEVYSNAP` v2 v1.x-compatible. Loading a v1.0 file in any v1.x kevy is guaranteed to work. |
| **RESP wire protocol** | All 94 commands in the parity table above keep their shape (arg count, reply type) for v1.x. New commands may be added; existing ones won't change. |
| **valkey-cli / redis-cli compat** | `redis-cli`, `valkey-cli`, redis-rs, go-redis, jedis, ioredis — all keep working unchanged across v1.x. |
| **Public Rust API** | `kevy_store::Store`, `kevy_embedded::Store`, `kevy_persist::Aof` / `RewriteStats`, `kevy_config::Config`, `kevy_rt::Runtime` / `Commands` — add-only across v1.x. Methods may gain optional params via new `*_with_*` variants; existing signatures stay. |
| **CLI flags + env vars** | `--bind` / `--port` / `--threads` / `--dir` / `--no-aof` / `--config`, `KEVY_BIND` / `KEVY_PORT` / `KEVY_THREADS` / `KEVY_DIR` / `KEVY_AOF` / `KEVY_CONFIG` — names + meanings stay across v1.x. |
| **TOML schema** | New `[section].key` fields allowed in v1.x; **no** rename or removal of existing fields until v2.0. Unknown keys are warned-not-errored, so older configs keep loading on newer kevy. |
| **Memory / eviction semantics** | The 8 eviction policy names (`noeviction` / `allkeys-{lru,lfu,random}` / `volatile-{lru,lfu,random,ttl}`) and their selection algorithms (24-bit clock, sample-based) stay. `maxmemory-samples = 5` is the v1.x default — tunable later via config. |

What's **not** covered:

- Performance numbers may improve; kevy targets the hardware ceiling
  every version.
- Internal crate organisation can change (e.g. a `kevy-rt` module split)
  without violating the API promise above.
- Debug output / log line format is best-effort, not contract.
