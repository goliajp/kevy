# kevy-client-async

The async mirror of [`kevy-client`](https://crates.io/crates/kevy-client).
The API surface mirrors blocking 1:1 — every method takes `.await`,
and the same URL backends are accepted.

```rust,no_run
use kevy_client_async::AsyncConnection;

# async fn run() -> std::io::Result<()> {
let mut conn = AsyncConnection::open("tcp://127.0.0.1:6379").await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
# Ok(())
# }
```

## Install

Pick exactly one runtime feature:

```toml
[dependencies]
kevy-client-async = { version = "1", features = ["tokio"] }
# or "smol", or "async-std"
```

Enabling zero or more than one runtime feature triggers a
`compile_error!`.

## Pipeline

Collapse N commands into one TCP round-trip:

```rust,no_run
use kevy_client_async::AsyncConnection;

# async fn run() -> std::io::Result<()> {
let mut conn = AsyncConnection::open("tcp://127.0.0.1:6379").await?;
let replies = conn.pipeline()
    .set(b"a", b"1")
    .get(b"a")
    .incr(b"hits")
    .run(&mut conn).await?;
# Ok(())
# }
```

## URL backends

Same set as the blocking client: `mem://`, `mem://<name>`,
`file:///abs/path`, `kevy://host:port`, `redis://host:port`,
`tcp://host:port`. See the [`kevy-client`
README](https://crates.io/crates/kevy-client) for the per-URL
semantics table.

## Why this is a separate crate

The kevy workspace is pure Rust with zero `crates.io` dependencies in
the default server, blocking-client, and embedded stacks. The Rust
async ecosystem has no `std`-only viable substrate, so the async
client is the single carved exemption: it may pull `tokio`, `smol`, or
`async-std` behind feature gates. The exemption is opt-in (you have to
add `kevy-client-async` to your `Cargo.toml`), and it does not enter
the default dependency graph of any other kevy crate.

## License

MIT OR Apache-2.0, at your option.
