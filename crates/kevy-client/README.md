# kevy-client

Unified KV facade for kevy — switch between **in-process embedded** and
**TCP server** backends with one URL string. Pure Rust, zero
`crates.io` runtime deps.

```rust
use kevy_client::Connection;

let mut conn = Connection::open(&std::env::var("MY_KEVY_URL").unwrap())?;
conn.set(b"hello", b"world")?;
assert_eq!(conn.get(b"hello")?, Some(b"world".to_vec()));
# Ok::<(), std::io::Error>(())
```

The same business code runs against any of:

| `MY_KEVY_URL` | Backend |
|---|---|
| `mem://` | in-process, in-memory only |
| `file:///var/lib/myapp/` | in-process, persistent (snapshot + AOF) |
| `kevy://prod-cache:6379` | TCP RESP server, kevy-native scheme |
| `redis://prod-cache:6379/0` | TCP RESP server, standard Redis URL |
| `tcp://prod-cache:6379` | TCP RESP server, raw (no SELECT round-trip) |

Auth (`redis://user:pass@…`) and TLS (`rediss://`) are rejected up
front — kevy ships without either; reach for stunnel / a proxy if you
need them at the network boundary.

## Install

```sh
cargo add kevy-client
```

## Why a facade

Without this crate the typical downstream config has two parallel
codepaths — one for "open an embedded `Store` with a path" and one for
"validate a `redis://` URL and open a TCP client". They share none of
their setup, error handling, or test fixtures. `Connection::open(url)`
replaces all of that with one builder.

The two backends were the kevy story anyway:

- **Embedded** (`kevy-embedded`): in-process, zero network, builds for
  `wasm32`. Use it for embedded caches and single-process apps.
- **Server** (`kevy` binary or Docker image): thread-per-core
  reactor + shared-nothing routing across cores + TCP RESP wire.

`kevy-client` ties both into one API so your app picks at runtime via
environment variable / config file — develop against `mem://`,
integration-test against `file:///tmp/test`, deploy against
`kevy://prod-cache:6379`. No code change.

## Command coverage (v1.5.0)

All five Redis data types plus generic-key ops, persistence, the full
pub/sub cycle (including in-process embedded delivery), multi-key
operations, scan/keys, and `MULTI`/`EXEC`/`DISCARD` transactions on the
remote backend. Methods on `Connection`:

**Connection / generic:** `ping`, `dbsize`, `flush`, `type_of`,
`exists`, `del`, `expire`, `persist`, `ttl_ms`.

**String:** `set`, `set_with_ttl`, `get`, `incr`, `incr_by`.

**Hash:** `hset`, `hget`, `hdel`, `hlen`, `hgetall`, `hkeys`, `hvals`.

**List:** `lpush`, `rpush`, `lpop`, `rpop`, `llen`, `lrange`.

**Set:** `sadd`, `srem`, `smembers`, `scard`, `sismember`.

**Sorted set:** `zadd`, `zrem`, `zscore`, `zcard`, `zrange`.

**Multi-key (v1.4.0):** `mget`, `mset`, `sinter`, `sunion`, `sdiff`.

**Keyspace iteration (v1.4.0):** `keys(pattern)`, `scan(cursor, pattern, count)`,
`randomkey`. Embedded `scan` finishes in one round (any non-zero cursor
returns empty); the remote backend honours the server's real cursor.

**Transactions (v1.4.0 + v1.5.0, remote only):** `conn.multi()` →
`Transaction` handle. Two queue surfaces — raw `queue(&[verb, args...])`
and v1.5.0's typed builders (`set`, `get`, `del`, `exists`, `incr`,
`incr_by`, `mget`, `mset`) that chain via `&mut Self`. Plus
[`Connection::watch`] / [`unwatch`] and [`Transaction::exec_watched`]
for optimistic concurrency. Embedded returns `ErrorKind::Unsupported` —
every Connection method already serialises on the embed mutex, so
MULTI's locking guarantee maps to a no-op there.

```rust
// raw shape, unchanged from v1.4.0
let mut txn = conn.multi()?;
txn.queue(&[b"SET", b"counter", b"0"])?;
txn.queue(&[b"INCR", b"counter"])?;
let replies = txn.exec()?;  // Vec<kevy_resp::Reply>

// v1.5.0: typed builders chain with `?` directly
let mut txn = conn.multi()?;
txn.set(b"a", b"1")?
    .incr(b"counter")?
    .del(&[b"tmp"])?;
let replies = txn.exec()?;

// v1.5.0: WATCH-driven optimistic concurrency
conn.watch(&[b"counter"])?;
let mut txn = conn.multi()?;
txn.incr(b"counter")?;
match txn.exec_watched()? {
    Some(replies) => assert_eq!(replies.len(), 1),
    None         => { /* watched key changed — retry the whole block */ }
}
```

**Pub/sub:** `Connection::publish` for the producer side. The consumer
side is `Subscriber`, a separate type with its own backing channel
because subscribed connections cannot send normal commands per the
RESP spec.

**v1.3.0 makes embed work the same way as the network**: two opens of the
same `mem://<name>` or `file:///path` URL route through a process-local
registry and share one backing `Store` + pub/sub bus. So the same code
runs against `mem://` in dev and `kevy://` in prod with **no scheme
branching**:

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let url = std::env::var("KEVY_URL").unwrap_or_else(|_| "mem://mailbus".into());
let mut sub = Subscriber::open(&url, &[b"news"])?;
let mut pubconn = Connection::open(&url)?;

// Drain the SUBSCRIBE ack first.
let _ack = sub.recv()?;

// Same URL → same bus, even across threads.
pubconn.publish(b"news", b"hello world")?;

if let PubsubEvent::Message { channel, payload } = sub.recv()? {
    println!("{}: {}", String::from_utf8_lossy(&channel),
                       String::from_utf8_lossy(&payload));
}
# Ok::<(), std::io::Error>(())
```

Anonymous `mem://` (no name) stays per-call isolated — `Subscriber::open`
rejects it with `ErrorKind::Unsupported` since no other producer can
reach it. Use `mem://<some-name>` for a shared bus.

If you need a command this crate doesn't expose yet, drop down to the
raw backend:

```rust
match &mut conn {
    kevy_client::Connection::Embedded(s) => { /* call any kevy_embedded::Store method */ }
    kevy_client::Connection::Remote(c)   => { /* call c.request(&[...]) directly */ }
}
```

## Same code, two backends — test pattern

```rust
use kevy_client::Connection;

fn cache_smoke(c: &mut Connection) -> std::io::Result<()> {
    c.set(b"hot", b"cached")?;
    assert_eq!(c.get(b"hot")?, Some(b"cached".to_vec()));
    Ok(())
}

#[test]
fn smoke_embedded() -> std::io::Result<()> {
    cache_smoke(&mut Connection::open("mem://")?)
}

# // Run when a kevy server is up at $TEST_KEVY:
#[test]
#[ignore]   // gated on $TEST_KEVY env var pointing at a running server
fn smoke_remote() -> std::io::Result<()> {
    let url = std::env::var("TEST_KEVY").unwrap();
    cache_smoke(&mut Connection::open(&url)?)
}
```

## License

MIT OR Apache-2.0, at your option.
