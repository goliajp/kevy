# kevy-client-async

The async mirror of [`kevy-client`](https://crates.io/crates/kevy-client).
The API surface mirrors blocking 1:1 — every method takes `.await`,
and the same URL backends are accepted.

```rust
use kevy_client_async::AsyncConnection;

# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/doc_serve.rs"));
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> std::io::Result<()> {
# let addr = serve(&[("SET k v", "+OK\r\n"), ("GET k", "$1\r\nv\r\n")]).await?;
let mut conn = AsyncConnection::connect(&format!("tcp://{addr}")).await?;
conn.set(b"k", b"v").await?;
let v = conn.get(b"k").await?;
assert_eq!(v.as_deref(), Some(&b"v"[..]));
# Ok(())
# }
```

## Install

Pick exactly one runtime feature:

```toml
[dependencies]
kevy-client-async = { version = "6", features = ["tokio"] }
# or "smol", or "async-std"
```

Enabling zero or more than one runtime feature triggers a
`compile_error!`.

## Pipeline

Collapse N commands into one TCP round-trip:

```rust
use kevy_client_async::AsyncConnection;
use kevy_resp::Reply;

# include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/doc_serve.rs"));
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> std::io::Result<()> {
# let addr = serve(&[
#     ("SET a 1", "+OK\r\n"),
#     ("GET a", "$1\r\n1\r\n"),
#     ("INCR hits", ":1\r\n"),
# ]).await?;
let mut conn = AsyncConnection::connect(&format!("tcp://{addr}")).await?;
let replies = conn.pipeline()
    .set(b"a", b"1")
    .get(b"a")
    .incr(b"hits")
    .run(&mut conn).await?;
assert_eq!(replies, [Reply::Simple(b"OK".to_vec()), Reply::Bulk(b"1".to_vec()), Reply::Int(1)]);
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
