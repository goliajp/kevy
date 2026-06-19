# Async client (`kevy-client-async`)

`kevy-client-async` is the runtime-agnostic async counterpart to
[`kevy-client`](https://docs.rs/kevy-client). The blocking client stays
the default for greenfield code (it's pure-Rust, 0-dep, and the lower
latency under non-async workloads). This crate exists for apps that
already have a `tokio` / `smol` / `async-std` runtime and want to keep
`await`-flow throughout — pipelining especially, which is where async
collapses N round-trips into one.

## When to use which

| You have…                                    | Use                  |
|----------------------------------------------|----------------------|
| no runtime, simple request-response code     | `kevy-client`        |
| a tokio app, want one `await` per command    | `kevy-client-async`  |
| a tokio app, want one `await` per batch      | `kevy-client-async` + `pipeline()` |
| any runtime, embedded `mem://` / `file://`   | `kevy-client`        |

`mem://` and `file://` URLs are rejected by `AsyncConnection::open` —
those are in-process synchronous backends; the blocking client is
strictly faster for them.

## Runtime selection

Exactly one of `tokio`, `smol`, `async-std` must be enabled.
Enabling zero or more than one triggers a compile-time error.

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["tokio"] }
```

Each runtime gets its own `TcpStream` adapter:

| feature       | transport                          |
|---------------|------------------------------------|
| `tokio`       | `tokio::net::TcpStream`            |
| `smol`        | `smol::net::TcpStream`             |
| `async-std`   | `async_std::net::TcpStream`        |

Each runtime dep ships with `default-features = false` plus the
minimum-surface features the adapter needs.

## Surface — mirror of blocking

```rust
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
```

The named methods on `AsyncConnection` are 1:1 with `kevy_client::Connection`
modulo `.await`. Migration from blocking is grep-replace
`Connection` → `AsyncConnection` plus an `.await` on every call.

Available command families (42 methods):

- **string + generic**: ping / set / get / del / exists / incr /
  incr_by / expire / persist / ttl_ms / type_of / dbsize / flushall /
  set_with_ttl / mget / mset / publish
- **hash**: hset / hget / hdel / hlen / hgetall / hkeys / hvals
- **list**: lpush / rpush / lpop / rpop / llen / lrange
- **set**: sadd / srem / smembers / scard / sismember / sinter /
  sunion / sdiff
- **sorted set**: zadd / zrem / zscore / zcard / zrange

## Pipeline-first sugar

This is where async actually pays off — single network round-trip per
batch instead of per command.

```rust
let replies = conn
    .pipeline()
    .set(b"k1", b"v1")
    .get(b"k2")
    .incr(b"counter")
    .run(&mut conn)
    .await?;
// replies: Vec<Reply>, one entry per queued command, in order.
```

Per-command errors land as `Reply::Error(_)` inside the returned `Vec`
— a single bad command does not tear down the batch. Outer `Err` is
reserved for connection-level failures (transport, malformed frame).

For commands not on the typed builder, use `push_raw(argv)`:

```rust
conn.pipeline()
    .push_raw(vec![b"CUSTOM".to_vec(), b"arg".to_vec()])
    .run(&mut conn).await?;
```

### Degrade path

`Pipeline::into_cmds()` returns `Vec<Vec<Vec<u8>>>` — the raw argv
batch. Feed them into a blocking client one at a time if you need to
fall back:

```rust
let cmds = conn.pipeline().get(b"a").set(b"b", b"v").into_cmds();
// On blocking kevy_client::Connection:
// for cmd in &cmds { blocking_conn.codec_mut().request(cmd)?; }
```

## Cluster client

`AsyncClusterClient` mirrors `kevy_client::ClusterClient` for
cluster-mode servers — one TCP connection per shard, CRC16 routing per
key, `-MOVED` never fires for correct routing.

```rust
use kevy_client_async::cluster::AsyncClusterClient;

let mut c = AsyncClusterClient::connect("127.0.0.1", 6004).await?;
c.set(b"user:42", b"…").await?;
```

## Subscriber

`AsyncSubscriber` mirrors `kevy_client::Subscriber` — a subscribed RESP
connection can't send normal commands so it's a separate type from
`AsyncConnection`. Drop-in for the blocking shape minus the
socket-level `set_read_timeout` (use your runtime's timeout primitive:
`tokio::time::timeout`, `async_io::Timer`, etc.).

```rust
use kevy_client_async::subscriber::AsyncSubscriber;

let mut sub = AsyncSubscriber::open("tcp://127.0.0.1:6004", &[b"ch"]).await?;
let (channel, payload) = sub.recv_message().await?;
```

## Errors

Every async method returns `std::io::Result<T>` using the same
`ErrorKind` mapping the blocking client uses:

| source                                | `ErrorKind`        |
|---------------------------------------|--------------------|
| RESP `-ERR …` reply                   | `Other`            |
| unexpected reply variant              | `Other`            |
| malformed RESP frame                  | `InvalidData`      |
| mid-read EOF                          | `UnexpectedEof`    |
| bad URL / port / scheme               | `InvalidInput`     |
| TLS / AUTH / embed URL scheme         | `Unsupported`      |
| raw socket I/O                        | (native kind)      |

Wider error context — the RESP error string, the unexpected
variant name — is in the `io::Error`'s message
(`.to_string()` / `.into_inner()`).

## Dep-rule exemption

`kevy-client-async` is the **only** crate in the kevy workspace
permitted to take a crates.io dep. The exemption is per-crate +
per-dep: `tokio`, `smol`, `async-std` are the only crates ever
pulled (each with an inline `# EXEMPTION` comment in `Cargo.toml`).
No other workspace crate may take `kevy-client-async` as a dep — that
would bleed the exemption transitively. Full rationale lives in the
v3-cluster RFC (F5) and the
`feedback-pure-rust-no-c-principle.md` memory.

## Examples

- [`tokio_hello`](../crates/kevy-client-async/examples/tokio_hello.rs)
  — open + ping + set/get + del.
- [`pipeline`](../crates/kevy-client-async/examples/pipeline.rs)
  — mixed batch in one round-trip.
