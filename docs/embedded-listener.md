# Embedded read-only RESP listener

An embedded (in-process) kevy store can expose itself to external
RESP clients — redis-cli, ops tooling, dashboards — over a read-only
listener. The store stays a library inside your process; the
listener is a window into it, not a second server: writes stay the
exclusive right of the owning process.

```rust
use kevy_embedded::{Config, Store};

fn main() -> kevy_embedded::KevyResult<()> {
    let store = Store::open(
        Config::default()
            .with_shards(4)
            .with_resp_listener("127.0.0.1:6009".parse().unwrap()),
    )?;
    store.hset(b"row:42", &[
        (b"state".as_slice(), b"live".as_slice()),
    ])?;
    // ... the application keeps running; clients can peek:
    Ok(())
}
```

```
$ redis-cli -p 6009 hgetall row:42
1) "state"
2) "live"
$ redis-cli -p 6009 scan 0 match 'row:*' count 100
$ kevy-cli -p 6009 DBSIZE
```

Any RESP client works — the listener speaks the same protocol as
the kevy server, both framed requests and inline commands (the
`redis-cli` PING style).

## Turning it on

- `Config::with_resp_listener(addr)` — one `SocketAddr`. **Off by
  default**; when off there is **no thread and no socket — zero
  tax** (gated: idle-listener write throughput within 10% of off,
  [`bench/topogate.sh`](../bench/topogate.sh)).
- The code is behind the `listener` cargo feature (on by default;
  not available on wasm32 targets).
- The listener holds only a weak handle: it never keeps the store
  alive, and connections end when the store drops.
- There is no authentication (deliberate: kevy has no AUTH plane).
  Bind loopback or a private interface; anything reachable can read
  everything the whitelist serves.

## Surface

Whitelist only — anything else answers `-ERR READONLY embedded
listener`:

```
PING ECHO GET MGET EXISTS TYPE TTL PTTL DBSIZE KEYS SCAN
HGET HMGET HGETALL HLEN LRANGE LLEN SMEMBERS SCARD SISMEMBER
ZSCORE ZCARD ZRANGE FEED.READ FEED.TAIL FEED.SHARDS INFO
```

That includes every write verb, `MULTI`, blocking pops, pub/sub,
and the extension planes (`IDX.*` / `VIEW.*` — query those
in-process through the typed API). The `FEED.*` trio needs the
`replicate` cargo feature (on by default).

`INFO` answers a small embedded-shaped report — crate version,
shard count, key count, and `listener:readonly` — enough for a
dashboard to identify what it is talking to.

The rejection text is a contract: tooling can probe "is this a real
server or an embedded window?" by attempting a write and matching
`READONLY embedded listener`.

### Verb semantics

The whitelisted verbs behave like their server counterparts:

- `SCAN <cursor> [MATCH pattern] [COUNT n]` pages with a numeric
  cursor (`COUNT` clamps to 1..=10000, default 100) and the usual
  SCAN guarantee: keys stable across the traversal are seen exactly
  once; concurrent inserts/deletes may or may not appear.
- `KEYS <pattern>` walks the whole keyspace in one reply (`*`
  matches everything) — spot checks only, prefer `SCAN` on large
  stores.
- Type mismatches answer `-WRONGTYPE …` exactly as the server does;
  malformed integers answer `-ERR value is not an integer`.

## Consistency

Reads run under the store's own shard locks — every reply is a
committed point-in-time answer, live with the writing process (no
replication, no lag, no snapshot staleness). Multi-key reads
(`MGET`, `SCAN`, `KEYS`, `DBSIZE`) merge per shard without a global
snapshot — the same SCAN-class envelope as the server.

One thread per connection: this is an ops-tooling surface, not a
serving path (the kevy *server* is the serving path). Keep the
connection count to tooling scale; a dashboard polling every few
seconds is the intended shape.

## Feeds and read-your-writes

`FEED.TAIL` returns the current `(generation, offset)`; `FEED.READ
<gen> <offset> <limit> [PREFIX p…]` delivers change frames — the same
at-least-once contract as the embedded `changes_since` API (a stale
generation answers `Resync`; restart from `FEED.TAIL`). The embedded
write path serializes all shards into one stream, so `FEED.SHARDS`
reports 1 and the feed verbs take no shard argument — the same
consumer loop written against the server surface
([cdc.md](cdc.md)) works here.

Cross-process read-your-writes over the feed is a cursor pattern, not
a blocking primitive: the writing process notes `changes_tail()`
after its write; the reading process first drains `FEED.READ` past
that cursor, then reads. In-process reads are always read-your-writes
(writes commit synchronously). (Server-to-replica replication *does*
have a blocking primitive — `REPL.TOKEN` / `REPL.WAIT`, see
[availability.md](availability.md) — but that is the replication
plane, not this feed listener.)

## Inspection without a socket

The listener's verb table is also a public method:

```rust
let mut out = Vec::new();
store.dispatch_readonly(
    &[b"HGETALL".to_vec(), b"row:42".to_vec()], &mut out);
// `out` holds raw RESP bytes — the exact reply the listener
// would have written to a socket.
```

`Store::dispatch_readonly(argv, out)` answers one request against
the same whitelist (write verbs get the same `-ERR`) — the
programmatic face of the listener, for tooling embedded in the
owning process.

And for a store you do NOT own: `kevy-cli --embed <dir>` opens a
read-only point-in-time view of an embedded store's data directory —
the dump/aof/shards.meta files are **copied** to a scratch dir and
replayed, so the owning process keeps running untouched. REPL and
one-shot both work; write verbs answer the listener's `-ERR
READONLY`. That is the offline complement to this listener's live
window.

## When you want more than a window

Escalation path, in order of machinery:

1. **This listener** — live reads, zero write cost, one process.
2. **The CDC feed** ([cdc.md](cdc.md)) — push changes to another
   process instead of having it poll reads.
3. **Embedded-as-primary replication** — `with_embed_writer` exposes
   a replication source, and a kevy server with `[replication]
   single_source = true` follows the embedded store as a replica:
   full read fan-out on server hardware, fed by your process. See
   the *Embedded-as-primary* section of
   [replication.md](replication.md).

## Performance

[`bench/topogate.sh`](../bench/topogate.sh) is the clamp, and it is
a TRUE two-process test — a writer binary under continuous HSET
load, a separate reader process asserting live data:

- Reader `GET` p99 < 1ms (median of 6 connections) while the owner
  writes continuously.
- READONLY enforcement (write verbs rejected with the contract
  text).
- Zero-tax: the owner's write throughput with the listener enabled
  but idle within 10% of listener-off.

The reads-under-shard-locks design means heavy listener traffic
CAN contend with the owner's writes on the same shards — that is
the trade for zero-lag truth. Tooling-scale read rates (dashboards,
spot checks) are unmeasurable in the owner's write throughput; if
you need serving-scale reads, use replication (above), not this
listener.

## See also

- [cdc.md](cdc.md) — the change feed this listener also serves.
- [replication.md](replication.md) — embedded-as-primary, when a
  window stops being enough.
- [availability.md](availability.md) — the consistency ladder on
  the replication plane.
- [uds.md](uds.md) — local-socket transport for the kevy server
  (the listener itself is TCP-only).
