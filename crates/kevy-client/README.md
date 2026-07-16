# kevy-client

A URL-driven, blocking RESP client for kevy. The same code switches
between an in-process backend and a remote TCP server by changing one
URL string.

- Pure Rust, zero `crates.io` runtime dependencies.
- Backends: in-process (anonymous, named, or persistent), kevy server,
  any Redis-protocol server.
- All five Redis data types, transactions with `WATCH`, scan iteration,
  and pub/sub with a borrowing iterator.

```rust
use kevy_client::Connection;

let mut conn = Connection::connect("tcp://127.0.0.1:6379")?;
conn.set(b"hello", b"world")?;
assert_eq!(conn.get(b"hello")?, Some(b"world".to_vec()));
# Ok::<(), kevy_client::KevyError>(())
```

## Install

```sh
cargo add kevy-client
```

## URL backends

| URL | Backend |
|---|---|
| `mem://` | Anonymous in-process, per-open fresh; no shared bus. |
| `mem://<name>` | Shared in-process bus keyed by `<name>` — two opens of the same name share one `Store` + pub/sub bus, even across threads. |
| `file:///abs/path` | Shared in-process with snapshot + AOF persistence in `path`. |
| `kevy://host:port` | TCP RESP server, kevy-native URL scheme. |
| `redis://host:port` | TCP RESP server, standard Redis URL. |
| `tcp://host:port` | TCP RESP, raw — no `SELECT` round-trip on connect. |

`redis://user:pass@host` (AUTH) and `rediss://` (TLS) are rejected up
front — kevy ships without either. Front it with a TLS-terminating
sidecar and an authentication proxy if you need them.

## Quick start

### Same code, two backends

```rust
use kevy_client::Connection;

fn cache_smoke(c: &mut Connection) -> kevy_client::KevyResult<()> {
    c.set(b"hot", b"cached")?;
    assert_eq!(c.get(b"hot")?, Some(b"cached".to_vec()));
    Ok(())
}

let url = std::env::var("KEVY_URL")
    .unwrap_or_else(|_| "mem://app".into());
cache_smoke(&mut Connection::connect(&url)?)?;
# Ok::<(), kevy_client::KevyError>(())
```

Set `KEVY_URL=mem://app` for dev, `KEVY_URL=kevy://prod:6379` for
production. No code change.

### Transactions

```rust
use kevy_client::Connection;

# fn run() -> kevy_client::KevyResult<()> {
let mut conn = Connection::connect("tcp://127.0.0.1:6379")?;

let mut txn = conn.multi()?;
txn.set(b"a", b"1")?
    .incr(b"counter")?
    .get(b"a")?;
let mut r = txn.exec_typed()?;
r.next_ok()?;                                 // SET → +OK
let counter: i64 = r.next_int()?;             // INCR → :N
let prior: Option<Vec<u8>> = r.next_bulk()?;  // GET  → $... or nil
r.expect_empty()?;                            // arity check
# Ok(())
# }
```

For optimistic concurrency, watch a key before the transaction:

```rust
use kevy_client::Connection;

# fn run() -> kevy_client::KevyResult<()> {
let mut conn = Connection::connect("tcp://127.0.0.1:6379")?;

conn.watch(&[&b"counter"[..]])?;
let mut txn = conn.multi()?;
txn.incr(b"counter")?;
match txn.exec_watched()? {
    Some(replies) => { /* committed */ }
    None         => { /* watched key changed — retry the whole block */ }
}
# Ok(())
# }
```

Transactions on the in-process backends return `ErrorKind::Unsupported`
— every `Connection` method already serialises on the embed mutex, so
`MULTI`'s locking guarantee is a no-op in-process.

### Pub/sub

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

# fn run() -> kevy_client::KevyResult<()> {
let url = std::env::var("KEVY_URL")
    .unwrap_or_else(|_| "mem://news".into());

let mut sub = Subscriber::connect_channels(&url, &[&b"updates"[..]])?;
let mut pubconn = Connection::connect(&url)?;

let _ack = sub.recv()?;                       // drain the SUBSCRIBE ack
pubconn.publish(b"updates", b"hello")?;

for event in sub.messages().take(1) {
    let (channel, payload) = event?;
    println!("{}: {}",
        String::from_utf8_lossy(&channel),
        String::from_utf8_lossy(&payload));
}
# Ok(())
# }
```

Pattern subscriptions use `psubscribe(&[&b"news.*"[..]])`.

`Subscriber::messages()` and `Subscriber::events()` are borrowing
iterators. The `messages()` form auto-skips the
`(p)?(un)?subscribe` ack frames and yields `(channel, payload)` tuples
directly. Both iterators terminate on `UnexpectedEof`; transient
errors surface as `Some(Err(_))` so callers decide whether to keep
going.

Anonymous `mem://` (no name) is rejected by `Subscriber::connect_channels` —
no other producer can reach it. Use `mem://<some-name>` for a shared
bus.

### Cluster-aware routing

`ClusterClient` discovers the topology of a cluster-mode kevy server
and routes each key straight to the owning shard, eliminating the
cross-shard forwarding hop:

```rust,no_run
use kevy_client::ClusterClient;

let mut cc = ClusterClient::connect("127.0.0.1", 6380)?;  // any shard port as seed
cc.set(b"user:42", b"alice")?;                             // routed by CRC16
let v = cc.get(b"user:42")?;
let removed = cc.del(&[&b"a"[..], &b"b"[..], &b"c"[..]])?; // multi-key may span shards
# Ok::<(), kevy_client::KevyError>(())
```

Full cluster-mode guide: [`docs/cluster.md`](https://github.com/goliajp/kevy/blob/develop/docs/cluster.md).

### Drop down to the raw backend

```rust
use kevy_client::Connection;

# fn handle(conn: &mut Connection) -> kevy_client::KevyResult<()> {
match conn {
    Connection::Embedded(s) => {
        // call any kevy_embedded::Store method
    }
    Connection::Remote(c)   => {
        // call c.request(&[...]) directly
    }
}
# Ok(())
# }
```

## Wrap coverage matrix (kevy server 4.x × kevy-client 2.0.0)

Status legend — **wrapped**: typed `Connection` methods; **partial**:
the listed subset is wrapped, the rest is raw-only; **raw-only**: no
typed wrap, reachable through the argv passthroughs
(`Connection::pipeline`, `Transaction::queue`, `idx_*_raw`, or
`Connection::Remote(c) => c.request(...)`); **n/a**: deliberately out
of scope for this client.

| Server op family (docs/verb-reference.md) | Status | Notes |
|---|---|---|
| connection (PING/ECHO/SELECT/HELLO…) | partial | `ping`; `SELECT` via URL `/db`; RESP3 `HELLO` n/a (client speaks RESP2) |
| server (INFO/CONFIG/DEBUG/COMMAND…) | raw-only | admin surface — use kevy-cli or the passthroughs |
| replication (REPLICAOF/WAIT/REPL.*) | raw-only | topology tooling, not a data-path client concern |
| scan (SCAN/KEYS/RANDOMKEY) | wrapped | `scan`, `keys`, `randomkey` |
| generic (DEL/EXPIRE/TTL/TYPE/RENAME/COPY…) | partial | `del`/`exists`/`expire`/`persist`/`ttl_ms`/`type_of`/`dbsize`/`flushall`; RENAME/COPY/TOUCH/GETEX… raw-only |
| tx (MULTI/EXEC/DISCARD/WATCH/UNWATCH) | wrapped | `multi` → `Transaction` (typed builders + typed reply cursor), `watch`, `unwatch`; remote-only |
| pipeline (client-side batch) | wrapped | `pipeline(|p| …)` — one write, N in-order replies; non-atomic; remote-only |
| pubsub (SUBSCRIBE/PUBLISH/PSUBSCRIBE…) | wrapped | `publish` + `Subscriber` (`recv`, `messages()`, patterns) |
| script (EVAL/EVALSHA…) | raw-only | |
| string (SET/GET/INCR/MSET/bitmaps…) | partial | `set`/`set_with_ttl`/`get`/`incr`/`incr_by`/`mget`/`mset`; APPEND/GETRANGE/SETNX/bitmaps raw-only |
| hash (HSET/HGET/HGETALL…) | partial | `hset`/`hget`/`hdel`/`hlen`/`hgetall`/`hkeys`/`hvals`; HINCRBY/HSETNX/HSCAN… raw-only |
| hash field-TTL (HEXPIRE/HPEXPIRE/HPERSIST/HTTL) | wrapped | v2.0.0: per-field codes in request order; `HPEXPIREAT` raw-only |
| list (LPUSH/LRANGE/LREM…) | partial | `lpush`/`rpush`/`lpop`/`rpop`/`llen`/`lrange`; LINSERT/LSET/LTRIM… raw-only |
| blocking (BLPOP/BRPOP/BZPOPMIN/BRPOPLPUSH) | partial | v2.0.0: `blpop`/`brpop`/`bzpopmin` (both backends really block); `BRPOPLPUSH` raw-only |
| set (SADD/SMEMBERS/SINTER…) | partial | `sadd`/`srem`/`smembers`/`scard`/`sismember`/`sinter`/`sunion`/`sdiff`; SPOP/SRANDMEMBER/S\*STORE raw-only |
| zset (ZADD/ZRANGE/ZSCORE…) | partial | `zadd`/`zrem`/`zscore`/`zcard`/`zrange`; ZCOUNT/ZRANK/ZINCRBY/ZSCAN… raw-only |
| zset algebra (Z\*STORE/ZINTERCARD) | partial | v2.0.0: `zinterstore`/`zunionstore` (+`_with` WEIGHTS/AGGREGATE)/`zintercard`; `ZDIFFSTORE` raw-only |
| stream (XADD/XREAD/XREADGROUP…) | raw-only | |
| geo (GEOADD/GEOSEARCH…) | raw-only | |
| index (IDX.CREATE/QUERY/DROP/LIST…) | partial | v2.0.0, remote-only: `idx_create_range`/`idx_create_raw`, `idx_query_range`/`_eq`/`_match`/`_knn`/`_raw`, `idx_drop`, `idx_list`; COUNT/VERIFY/EXPLAIN/REBUILD via `idx_query_raw`-style passthrough |
| view (VIEW.*) | raw-only | |
| feed / CDC (FEED.SHARDS/TAIL/READ) | wrapped | v2.0.0: `feed_shards`/`feed_tail`/`feed_read` — the network face of embedded `changes_since`; same `FEEDRESYNC` error text on both backends |
| migration (RESTORE/MIGRATE…) | raw-only | |
| cluster (CLUSTER SLOTS routing) | wrapped | `ClusterClient` (separate type, CRC16 slot routing) |

The embedded backend serves every wrapped family except the
remote-only rows (tx, pipeline, index); for those it answers
`ErrorKind::Unsupported` and you drop down to `Connection::Embedded`'s
`kevy_embedded::Store` typed API.

## API surface

**Connection / generic**: `ping`, `dbsize`, `flush`, `type_of`,
`exists`, `del`, `expire`, `persist`, `ttl_ms`.

**Strings**: `set`, `set_with_ttl`, `get`, `incr`, `incr_by`.

**Hashes**: `hset`, `hget`, `hdel`, `hlen`, `hgetall`, `hkeys`,
`hvals`.

**Lists**: `lpush`, `rpush`, `lpop`, `rpop`, `llen`, `lrange`.

**Sets**: `sadd`, `srem`, `smembers`, `scard`, `sismember`.

**Sorted sets**: `zadd`, `zrem`, `zscore`, `zcard`, `zrange`.

**Multi-key**: `mget`, `mset`, `sinter`, `sunion`, `sdiff`.

**Blocking pops** (v2.0.0): `blpop`, `brpop`, `bzpopmin` — real
blocking on both backends; `timeout: Option<Duration>` (`None` =
forever; the remote socket has no read timeout, so a blocking pop
never races a client-side deadline).

**Hash field-TTL** (v2.0.0): `hexpire`, `hpexpire`, `hpersist`,
`httl` — per-field result codes (`HExpireCode`) in request order,
with the `NX|XX|GT|LT` condition as `HExpireCond`.

**Zset algebra** (v2.0.0): `zinterstore`, `zunionstore` (plus `_with`
variants exposing `WEIGHTS` + `AGGREGATE SUM|MIN|MAX`), `zintercard`
with optional `LIMIT`.

**Declarative indexes** (v2.0.0, remote-only): `idx_create_range`,
`idx_create_raw`, `idx_query_range` / `idx_query_eq` (paged
`IdxPage`), `idx_query_match` / `idx_query_knn` (ranked `(key,
score)`), `idx_query_raw`, `idx_drop`, `idx_list` (`IdxInfo`).

**Change feed / CDC** (v2.0.0): `feed_shards`, `feed_tail`,
`feed_read` (`FeedBatch` of offset-tagged argv frames, prefix
filtering, `FEEDRESYNC` cursor-rebuild contract shared with the
embedded `changes_since`).

**Pipelining** (v2.0.0, remote-only): `pipeline(|p| p.cmd(...))` —
one write, in-order replies, non-atomic.

**Keyspace iteration**: `keys(pattern)`,
`scan(cursor, pattern, count)`, `randomkey`. In-process backends
finish in one round (any non-zero cursor returns empty); the remote
backend honours the server's real cursor.

**Transactions** (remote only): `Connection::multi` returns a
`Transaction`. Queue commands via the typed builders (`set`, `get`,
`del`, `exists`, `incr`, `incr_by`, `mget`, `mset`), commit with
`exec`, `exec_typed`, `exec_watched`, or `exec_watched_typed`. The
typed cursor (`exec_typed`) hands back a `TransactionReplies` with
`next_ok` / `next_int` / `next_bulk` / `next_array_of_bulks` /
`expect_empty`.

**Pub/sub**: `Connection::publish` for the producer side;
`Subscriber` for the consumer side, with `recv`, `recv_message`,
`subscribe`, `unsubscribe`, `psubscribe`, `punsubscribe`, and the
`messages()` / `events()` iterators.

**Cluster routing**: `ClusterClient::connect` discovers topology and
routes by CRC16 slot.

## Async mirror

For applications already running on `tokio`, `smol`, or `async-std`,
use [`kevy-client-async`](https://crates.io/crates/kevy-client-async)
— the API surface mirrors `kevy-client` exactly, with `.await` on
every call.

## License

MIT OR Apache-2.0, at your option.
