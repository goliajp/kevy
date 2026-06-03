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

## Command coverage (v1.0.5)

The first cut covers the **string + generic-key subset** — about 80%
of cache-style use. Methods on `Connection`:

| Method | Redis equivalent |
|---|---|
| `ping()` | `PING` |
| `set(k, v)` | `SET key value` |
| `get(k)` | `GET key` |
| `del(&[k1, k2])` | `DEL k1 k2` |
| `exists(&[k1, k2])` | `EXISTS k1 k2` |
| `incr(k)` | `INCR k` |
| `incr_by(k, delta)` | `INCRBY k delta` |
| `expire(k, ttl)` | `PEXPIRE k ttl_ms` |
| `persist(k)` | `PERSIST k` |
| `ttl_ms(k)` | `PTTL k` (-2 absent, -1 no TTL) |
| `type_of(k)` | `TYPE k` |
| `dbsize()` | `DBSIZE` |
| `flush()` | `FLUSHDB` |

**v1.1.0** will round out hash / list / set / sorted-set / pub-sub. If
you need a command this crate doesn't expose yet, drop down to the raw
backend:

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
