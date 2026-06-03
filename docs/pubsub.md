# Pub/sub in kevy-client (v1.3.0)

The same code drives both an in-process bus and a TCP kevy server.
Pick the backend with a URL at runtime — no scheme-branching at call
sites.

```toml
[dependencies]
kevy-client = "1.3.0"
```

## URL semantics

| URL | Backend | Shared across opens? |
|---|---|---|
| `mem://` | in-process, in-memory | **No** — fresh each open |
| `mem://<name>` | in-process, in-memory | **Yes** — same `<name>` → same bus |
| `file:///abs/path` | in-process + snapshot/AOF persistence | **Yes** — same path → same bus |
| `kevy://host[:port][/db]` | TCP kevy/Redis server | (one socket per open, server-side fan-out) |
| `redis://host[:port][/db]` | TCP — alias of `kevy://` | same |
| `tcp://host[:port]` | TCP — raw, no leading `SELECT` | same |

`rediss://` / `kevys://` / `redis://user:pass@…` are rejected with
`ErrorKind::Unsupported` — kevy ships without TLS or AUTH.

**Anonymous `mem://` cannot receive published messages** because
nothing else can reach the same backing `Store`. `Subscriber::open`
rejects it with `ErrorKind::Unsupported`. Use `mem://<some-name>`.

## Pattern 1 — same-thread dev loop

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub  = Subscriber::open("mem://app", &[b"news"])?;
let mut conn = Connection::open("mem://app")?;

// Drain the SUBSCRIBE ack before publishing — the bus is ordered,
// but the ack arrives ahead of the first Message in the queue.
let _ack = sub.recv()?;

conn.publish(b"news", b"hello")?;

if let PubsubEvent::Message { channel, payload } = sub.recv()? {
    assert_eq!(channel, b"news");
    assert_eq!(payload, b"hello");
}
# Ok::<(), std::io::Error>(())
```

## Pattern 2 — cross-thread producer / consumer

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};
use std::thread;

const URL: &str = "mem://orders";

let mut sub = Subscriber::open(URL, &[b"order.placed"])?;
let _ack = sub.recv()?;

thread::spawn(|| {
    let mut conn = Connection::open(URL).unwrap();
    conn.publish(b"order.placed", b"order-42").unwrap();
});

let ev = sub.recv()?;
// PubsubEvent::Message { channel: "order.placed", payload: "order-42" }
# Ok::<(), std::io::Error>(())
```

## Pattern 3 — environment-driven dev/prod swap

The whole point of v1.3.0 — same code, two backends:

```rust
use kevy_client::{Connection, Subscriber};

fn run_app(url: &str) -> std::io::Result<()> {
    let mut sub  = Subscriber::open(url, &[b"jobs"])?;
    let mut conn = Connection::open(url)?;
    let _ack = sub.recv()?;
    conn.publish(b"jobs", b"compute pi")?;
    // ... drain events ...
    Ok(())
}

// Dev:
run_app("mem://app")?;
// Tests with persistence:
run_app("file:///tmp/app-test")?;
// Prod:
run_app("kevy://prod-cache:6379")?;
# Ok::<(), std::io::Error>(())
```

No `match scheme { ... }` at any call site. Open one URL, both ends
attach to the same backing bus.

## Pattern 4 — glob patterns

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub = Subscriber::connect("mem://signals")?;
sub.psubscribe(&[b"sensor.*"])?;
let _ack = sub.recv()?;  // Psubscribe ack

let mut conn = Connection::open("mem://signals")?;
conn.publish(b"sensor.temp", b"22.5")?;  // matches
conn.publish(b"weather", b"sunny")?;     // does NOT match

if let PubsubEvent::Pmessage { pattern, channel, payload } = sub.recv()? {
    assert_eq!(pattern, b"sensor.*");
    assert_eq!(channel, b"sensor.temp");
    assert_eq!(payload, b"22.5");
}
# Ok::<(), std::io::Error>(())
```

Glob syntax: `*` (any), `?` (one char), `[abc]` (char class) — the same
matcher as `KEYS` / `SCAN`.

## Pattern 5 — fan-out to multiple subscribers

```rust
use kevy_client::{Connection, Subscriber};

const URL: &str = "mem://fanout";
let mut s1 = Subscriber::open(URL, &[b"chan"])?;
let mut s2 = Subscriber::open(URL, &[b"chan"])?;
let _ = s1.recv()?;
let _ = s2.recv()?;

let mut conn = Connection::open(URL)?;
let received = conn.publish(b"chan", b"broadcast")?;
assert_eq!(received, 2);  // both got it
# Ok::<(), std::io::Error>(())
```

## API summary

```rust
// Producer
let mut conn = Connection::open(url)?;
let recv_count = conn.publish(channel, payload)?;

// Consumer
let mut sub = Subscriber::open(url, &[channel])?;          // open + subscribe
// or
let mut sub = Subscriber::connect(url)?;                    // open, subscribe later
sub.subscribe(&[chan1, chan2])?;
sub.psubscribe(&[b"foo.*"])?;
sub.unsubscribe(&[chan1])?;       // empty &[] → unsubscribe all channels
sub.punsubscribe(&[])?;            // empty &[] → unsubscribe all patterns
sub.set_read_timeout(Some(Duration::from_secs(1)))?;
let ev: PubsubEvent = sub.recv()?;
```

`PubsubEvent` carries six variants: `Subscribe`, `Psubscribe`,
`Unsubscribe`, `Punsubscribe`, `Message`, `Pmessage`. `Unsubscribe` /
`Punsubscribe` use `Option<Vec<u8>>` for the channel/pattern slot —
`None` matches the "no channels were subscribed" nil-bulk wire shape.

## Lifecycle + gotchas

**Process-local registry.** The URL → `Store` map is per-process,
backed by `Weak` refs. When the last `Connection` / `Subscriber` for a
named URL drops, the entry frees; the next open of the same URL gets
a fresh `Store`. (For `file:///` URLs the on-disk AOF + snapshot stays;
re-open replays.)

**Cross-process.** `mem://name` and `file:///path` are **not**
visible from another process. For real cross-process delivery, run a
kevy server and use `kevy://host:port`.

**Ack ordering.** `SUBSCRIBE` enqueues a `Subscribe` ack on the
receive queue before any `Message` for that channel. Drain the ack
before asserting on message bodies in tests.

**Send timing.** The bus mutex is dropped before `Sender::send()` is
called, so a slow receiver can't stall publishes on unrelated
channels. Each subscriber has its own `mpsc::Receiver` queue (no
shared bound).

**`Subscription` drop unregisters atomically.** No "stale subscriber"
zombie state if a thread panics — the `Drop` impl walks the bus
tables and removes every entry tagged with the subscription id.

**`Connection::publish` on anonymous `mem://`** returns 0 forever
(no possible subscribers). On `mem://<name>` it returns the real
receiver count.

**TLS / AUTH** are not supported. Front with stunnel + IP allowlist
at the network boundary if you need them.

## Migrating from v1.2.0

Source-compatible. The semantic change is `Connection::publish` on
embedded URLs:

| URL | v1.2.0 publish | v1.3.0 publish |
|---|---|---|
| `mem://` | always `Ok(0)` | always `Ok(0)` (unchanged) |
| `mem://<name>` | error | `Ok(<recipient count>)` |
| `file:///path` | always `Ok(0)` | `Ok(<recipient count>)` |
| `kevy://…` | `Ok(<server count>)` | `Ok(<server count>)` (unchanged) |

`Subscriber::open` accepts `mem://<name>` and `file:///path` in v1.3.0
(both returned `Unsupported` before). `mem://` (anonymous) still
returns `Unsupported`.

## Related

- [`kevy-embedded` v1.1.0](https://crates.io/crates/kevy-embedded) —
  the underlying `Store::Clone` + `PubsubBus` primitives. Use directly
  if you don't need the URL-facade indirection.
- [`kevy-client` v1.3.0](https://crates.io/crates/kevy-client) — the
  URL facade itself.
- [`kevy`](https://crates.io/crates/kevy) — the TCP server, when you
  outgrow single-process.
