# Embedded read-only RESP listener

An embedded (in-process) kevy store can expose itself to external
RESP clients — redis-cli, ops tooling, dashboards — over a read-only
listener:

```rust
let store = Store::open(
    Config::default()
        .with_shards(4)
        .with_resp_listener("127.0.0.1:6009".parse().unwrap()),
)?;
```

```
$ redis-cli -p 6009 hgetall row:42
$ redis-cli -p 6009 scan 0 match 'row:*' count 100
```

Off by default; when off there is **no thread and no socket — zero
tax** (gated: idle-listener write throughput within 10% of off).
The listener holds only a weak handle: it never keeps the store
alive, and connections end when the store drops.

## Surface

Whitelist only — anything else answers `-ERR READONLY embedded
listener`:

```
PING ECHO GET MGET EXISTS TYPE TTL PTTL DBSIZE KEYS SCAN
HGET HMGET HGETALL HLEN LRANGE LLEN SMEMBERS SCARD SISMEMBER
ZSCORE ZCARD ZRANGE FEED.READ FEED.TAIL FEED.SHARDS INFO
```

Reads run under the store's own shard locks — every reply is a
committed point-in-time answer, live with the writing process (no
replication, no lag). One thread per connection: this is an
ops-tooling surface, not a serving path (the kevy *server* is the
serving path).

## Feeds and read-your-writes

`FEED.TAIL` returns the current `(generation, offset)`; `FEED.READ
<gen> <offset> <limit> [PREFIX p…]` delivers change frames — the same
at-least-once contract as the embedded `changes_since` API (a stale
generation answers `Resync`; restart from `FEED.TAIL`).

Cross-process read-your-writes over the feed is a cursor pattern, not
a blocking primitive: the writing process notes `changes_tail()`
after its write; the reading process first drains `FEED.READ` past
that cursor, then reads. In-process reads are always read-your-writes
(writes commit synchronously). (Server-to-replica replication *does*
have a blocking primitive — `REPL.TOKEN` / `REPL.WAIT`, see
[availability.md](availability.md) — but that is the replication
plane, not this feed listener.)

Embedded-as-primary replication has since shipped (v3.2) on an
adjacent surface: `with_embed_writer` exposes a replication source,
and a kevy server with `[replication] single_source = true` follows
it as a replica — see the *Embedded-as-primary* section of
[replication.md](replication.md).

## Gate

`bench/topogate.sh`: true two-process — a writer binary under
continuous HSET load, a reader process asserting live data, GET p99
< 1ms (median of 6 connections), READONLY enforcement, and the
zero-tax clamp.
