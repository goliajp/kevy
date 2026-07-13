# Bring your redis client

kevy's server speaks RESP — the wire protocol every Redis-adjacent
ecosystem already has a mature client for. There is no kevy-specific
client to install (Rust excepted, below): connect the client you
already use, and the full verb surface — including kevy's extensions
(`IDX.*`, `VIEW.*`, `FEED.*`) — is reachable through its raw-command
channel.

This page is not a hope: **clientgate** runs in CI on every push. Six
clients each execute the same ladder against one kevy server — typed
operations across the string / TTL / hash / list / zset families, a
pub/sub round trip, and an extended verb through the raw channel.

| Ecosystem | Client | Raw channel for `IDX.*` etc. |
|---|---|---|
| Node | [node-redis](https://www.npmjs.com/package/redis) | `client.sendCommand([...])` |
| Node | [ioredis](https://www.npmjs.com/package/ioredis) | `client.call(...)` |
| Go | [go-redis](https://github.com/redis/go-redis) | `client.Do(ctx, ...)` |
| .NET | [StackExchange.Redis](https://www.nuget.org/packages/StackExchange.Redis) | `db.Execute(...)` |
| Python | [redis-py](https://pypi.org/project/redis/) | `client.execute_command(...)` |
| C | [hiredis](https://github.com/redis/hiredis) | `redisCommand(...)` |

Async comes with the ecosystem: ioredis, go-redis, StackExchange.Redis
and redis-py's `asyncio` face are all async-native — kevy needs no
special client for it.

```js
// Node — ioredis
import Redis from "ioredis";
const db = new Redis(6379);
await db.set("k", "v");
await db.call("IDX.CREATE", "idx_age", "ON", "PREFIX", "user:", "FIELD", "age", "TYPE", "i64", "KIND", "range");
```

```go
// Go — go-redis
c := redis.NewClient(&redis.Options{Addr: "localhost:6379"})
c.Set(ctx, "k", "v", 0)
c.Do(ctx, "IDX.CREATE", "idx_age", "ON", "PREFIX", "user:", "FIELD", "age", "TYPE", "i64", "KIND", "range")
```

```python
# Python — redis-py
r = redis.Redis()
r.set("k", "v")
r.execute_command("IDX.CREATE", "idx_age", "ON", "PREFIX", "user:", "FIELD", "age", "TYPE", "i64", "KIND", "range")
```

## Rust

Rust gets first-party typed clients, because that is where the typed
extension surface lives natively:

- [`kevy-client`](https://crates.io/crates/kevy-client) — synchronous,
  the full wrapped surface (indexes, views, feeds, pipelining,
  MULTI/EXEC + WATCH).
- [`kevy-client-async`](https://crates.io/crates/kevy-client-async) —
  the same surface on async I/O.

The smoke sources behind the gate live in
[`bench/clientgate/`](../bench/clientgate/) — each doubles as a
copy-paste connection example for its ecosystem.
