# Async client

`kevy-client-async` is the async mirror of the blocking [`kevy-client`](https://github.com/goliajp/kevy/tree/develop/crates/kevy-client) — same surface, same URL facade, with `.await` on every call.

## When you need this

Reach for the async client when your app already runs on a `tokio`, `smol`, or `async-std` runtime and you want `await`-flow end-to-end: no blocking threadpool hops, no `spawn_blocking` wrapping, no thread-per-connection. If your code path is request-response on a regular thread, the blocking client is simpler and lower-latency — there is no async tax to pay for being synchronous.

## Core idea

Pick exactly one runtime via a Cargo feature (`tokio`, `smol`, or `async-std`); the crate compiles down to that runtime's `TcpStream` adapter and nothing else. The public surface mirrors the blocking client 1:1 — `AsyncConnection::open(url).await?`, `conn.set(k, v).await?`, `conn.get(k).await?` — so porting from blocking is `Connection` → `AsyncConnection` plus an `.await` per call. A pipeline builder collapses N commands into one TCP round-trip when latency matters.

## Worked examples

### Tokio

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["tokio"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net"] }
```

```rust
use kevy_client_async::AsyncConnection;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
    conn.set(b"k", b"v").await?;
    let v = conn.get(b"k").await?;
    assert_eq!(v.as_deref(), Some(&b"v"[..]));
    Ok(())
}
```

### Smol

Same code; swap the runtime feature.

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["smol"] }
smol = "2"
```

```rust
use kevy_client_async::AsyncConnection;

fn main() -> std::io::Result<()> {
    smol::block_on(async {
        let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
        conn.set(b"k", b"v").await?;
        let v = conn.get(b"k").await?;
        assert_eq!(v.as_deref(), Some(&b"v"[..]));
        Ok(())
    })
}
```

### Pipeline builder

One round-trip for the whole batch. Replies come back in queue order; per-command failures land as `Reply::Error(_)` inside the `Vec` rather than tearing down the batch.

```rust
use kevy_client_async::AsyncConnection;

let mut conn = AsyncConnection::open("tcp://127.0.0.1:6004").await?;
let replies = conn
    .pipeline()
    .set(b"a", b"1")
    .get(b"a")
    .incr(b"hits")
    .run(&mut conn)
    .await?;
// replies.len() == 3; one Reply per queued command, in order.
```

## Runtime features

Exactly one of these must be enabled. Zero features, or two-plus features at once, is a compile-time error — there is no implicit default.

| feature      | transport adapter                  | runtime crate pulled |
|--------------|------------------------------------|----------------------|
| `tokio`      | `tokio::net::TcpStream`            | `tokio`              |
| `smol`       | `smol::net::TcpStream`             | `smol`               |
| `async-std`  | `async_std::net::TcpStream`        | `async-std`          |

Each runtime crate is pulled with `default-features = false` plus the minimum surface the adapter needs. These are the only crates.io dependencies in the kevy workspace — a deliberate carved exemption to the pure-Rust, zero-dependency rule, because the Rust async ecosystem has no std-only viable substrate.

## URL backends

`AsyncConnection::open` takes the same URL facade as the blocking client. The TCP-shaped schemes go over the runtime's async socket; the in-process schemes are rejected (the blocking client is strictly faster for them — no point routing through an executor).

| scheme       | target                          | supported by async client |
|--------------|---------------------------------|---------------------------|
| `tcp://`     | kevy or Redis-compat server     | yes                       |
| `kevy://`    | kevy server (alias of `tcp://`) | yes                       |
| `redis://`   | Redis or Redis-compat server    | yes                       |
| `mem://`     | in-process embedded store       | no — use blocking client  |
| `file:///`   | on-disk embedded store          | no — use blocking client  |

Opening a `mem://` or `file:///` URL with `AsyncConnection::open` returns `ErrorKind::Unsupported`.

## Trade-offs

The blocking client is the default and stays the default for a reason:

- **Sync code paths**: if you do not already have a runtime, do not stand one up for the client. `kevy-client` is pure-Rust, zero-dep, and avoids the executor's scheduling overhead on every command.
- **Embedded backends**: `mem://` and `file:///` are synchronous in-process stores. The blocking client talks to them directly; the async client cannot.
- **Single-shot commands**: one `.await` per command on a stock multi-threaded executor is measurable overhead vs. a direct syscall. The async win shows up under concurrency (many in-flight commands across tasks) or batching (pipeline collapsing round-trips).

Use async when the surrounding app is already async. Use the pipeline builder when you have a batch of independent commands and the round-trip is the bottleneck. Stay on blocking otherwise.

## FAQ

**Why must I pick exactly one runtime?**
The crate compiles a single `TcpStream` adapter. Two adapters in one binary would mean either runtime-agnostic indirection on every I/O (overhead) or a giant cfg matrix nobody can maintain. Zero adapters would leave the public types unimplemented. A compile-time check on feature count keeps the misconfiguration loud and early.

**Can I mix sync and async kevy clients in one process?**
Yes. `kevy-client` (blocking) and `kevy-client-async` are independent crates and coexist freely — use blocking for an embedded `file:///` store and async for a network shard from the same binary, for instance. They do not share connections.

**What about pub/sub?**
`AsyncSubscriber` mirrors the blocking `Subscriber`. A subscribed RESP connection cannot send normal commands, so it is a separate type from `AsyncConnection`. Per-message timeouts use your runtime's own primitive (`tokio::time::timeout`, `async_io::Timer`, etc.) rather than a socket-level read timeout.

**Does the pipeline builder force buffering on the send side?**
Yes — that is the point. `pipeline().…run(&mut conn).await` serializes the whole batch into one write and reads N replies in order. If you need command-by-command back-pressure, call `set` / `get` directly instead of building a pipeline.

## Examples in the repo

- [`tokio_hello`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/tokio_hello.rs) — open, ping, set/get, del.
- [`pipeline`](https://github.com/goliajp/kevy/blob/develop/crates/kevy-client-async/examples/pipeline.rs) — mixed batch in one round-trip.
