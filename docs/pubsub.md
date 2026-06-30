# Pub/sub

How publishers fan messages out to many subscribers in kevy — over the
wire with `PUBLISH` / `SUBSCRIBE`, in-process via the embedded `Store`,
and through the same URL facade that the rest of `kevy-client` uses.

## When you need this

Reach for pub/sub when one writer needs to notify zero-or-more readers
*right now*, and you do not care about messages that arrive while a
reader is offline:

- "Tell every web worker to refresh its config cache."
- "Stream just-written rows from one shard to whoever is tailing."
- "Wake a worker pool when a job lands; the job itself is in a list."
- "Dev loop: a producer thread and a consumer thread in the same
  binary, no Redis box required."

If you need durable hand-off (job queue with retries, fan-out over
restarts, message replay), use a list or stream instead — see
[`docs/persistence.md`](https://github.com/goliajp/kevy/blob/develop/docs/persistence.md)
for what gets written to disk.

## Core idea

A pub/sub channel is a name. Subscribers register interest in that
name (or a glob pattern); a publish on the same name walks the
subscriber index and enqueues one copy of the body per matching
subscriber. There is no broker queue, no offline buffer, no ack — if
nobody is listening the moment you publish, the message is gone.

```
                   publish("news", body)
                          |
                          v
             +-----------------------+
             |  channel "news"       |   <- per-channel subscriber index
             |  subscribers: [A,B,C] |
             +-----------------------+
                  |       |       |
                  v       v       v
               sub A   sub B   sub C    <- each gets its own copy
```

Internally each publish builds the wire frame once, wraps the body in
an `Arc`, and uses `writev` to scatter-gather it to every matching
TCP subscriber — so the body bytes are copied **zero** extra times
no matter how wide the fan-out. The same per-channel index handles
both server connections and in-process `Subscription` handles.

## Worked examples

### Smoke-test with `redis-cli`

Open two shells against a running kevy server:

```sh
# shell 1 — subscriber
$ redis-cli -p 6379 SUBSCRIBE news
Reading messages... (press Ctrl-C to quit)
1) "subscribe"
2) "news"
3) (integer) 1
```

```sh
# shell 2 — publisher
$ redis-cli -p 6379 PUBLISH news "hello"
(integer) 1   # one subscriber received it
```

Back in shell 1:

```
1) "message"
2) "news"
3) "hello"
```

A `PUBLISH` to a channel with no subscribers returns `(integer) 0`
and the message is dropped on the floor. That is the contract — you
do not get a "we tried to deliver this" signal.

### Rust over the URL facade — `kevy-client`

The same call shape targets a TCP server, a named in-process bus, or
a persistent in-process store; flip the URL and recompile, no
`match scheme { … }` at call sites.

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

fn run(url: &str) -> std::io::Result<()> {
    // Open a subscriber against `news`. The first frame the bus
    // hands back is the subscribe ack; drain it before asserting
    // on bodies.
    let mut sub = Subscriber::open(url, &[b"news"])?;
    let _ack = sub.recv()?;

    let mut conn = Connection::open(url)?;
    let received = conn.publish(b"news", b"hello")?;
    assert_eq!(received, 1);

    match sub.recv()? {
        PubsubEvent::Message { channel, payload } => {
            assert_eq!(channel, b"news");
            assert_eq!(payload, b"hello");
        }
        other => panic!("unexpected frame: {other:?}"),
    }
    Ok(())
}

// Dev:  in-process shared bus by name.
run("mem://app")?;
// Prod: real TCP server.
run("kevy://prod-cache:6379")?;
# Ok::<(), std::io::Error>(())
```

Cross-thread is the same code with one `Subscriber` and one
`Connection` opened against the same URL from different threads —
the `mem://<name>` registry hands both ends the same backing bus, so
the producer thread can `Connection::publish` and the consumer
thread blocks in `sub.recv()`.

### In-process via `kevy-embedded`

When the embedding code already has a `Store`, skip the URL
indirection and talk to the bus directly:

```rust
use kevy_embedded::{Config, PubsubFrame, Store};

let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// Subscriber owns the receive queue.
let sub = store.subscribe(&[b"jobs"]);
let _ack = sub.recv()?; // PubsubFrame::Subscribe

// Any clone of `store` reaches the same bus.
let writer = store.clone();
assert_eq!(writer.publish(b"jobs", b"compute-pi"), 1);

match sub.recv()? {
    PubsubFrame::Message { channel, payload } => {
        assert_eq!(channel, b"jobs");
        assert_eq!(payload, b"compute-pi");
    }
    other => panic!("unexpected frame: {other:?}"),
}
# Ok::<(), std::io::Error>(())
```

`Store::clone` is cheap (it's an `Arc` bump), so the common shape is
"hand each thread a `store.clone()` and let it `publish` or
`subscribe` whenever it needs to." Subscribers drop unregisters
atomically; a panicking consumer thread does not leave a zombie
entry in the index.

### Pattern subscriptions

`PSUBSCRIBE` registers a glob and receives messages on every channel
that matches it. The glob syntax — `*`, `?`, `[abc]` — is the same
matcher `KEYS` and `SCAN` use.

```rust
use kevy_client::{Connection, Subscriber, PubsubEvent};

let mut sub = Subscriber::connect("mem://signals")?;
sub.psubscribe(&[b"news.*"])?;
let _ack = sub.recv()?;            // PubsubEvent::Psubscribe

let mut conn = Connection::open("mem://signals")?;
conn.publish(b"news.tech", b"breaking")?; // matches
conn.publish(b"weather",   b"sunny")?;    // does NOT match

match sub.recv()? {
    PubsubEvent::Pmessage { pattern, channel, payload } => {
        assert_eq!(pattern, b"news.*");
        assert_eq!(channel, b"news.tech");
        assert_eq!(payload, b"breaking");
    }
    other => panic!("unexpected frame: {other:?}"),
}
# Ok::<(), std::io::Error>(())
```

A subscriber that holds both a channel subscription **and** a
matching pattern subscription receives **two** copies — one
`Message`, one `Pmessage`. Per-publish dedup only suppresses the
"same `Subscription` listed twice in the same channel index"
duplicate, not channel-vs-pattern overlap.

## URL backend table

| URL                                | Backing store              | Shared across opens?                              | Cross-process visible? |
|------------------------------------|----------------------------|---------------------------------------------------|-----------------------|
| `mem://`                           | in-process, anonymous      | **No** — each open gets a fresh `Store`           | No                    |
| `mem://<name>`                     | in-process, named registry | **Yes** — same `<name>` ⇒ same `Store`            | No                    |
| `file:///abs/path`                 | in-process + AOF/snapshot  | **Yes** — same path ⇒ same `Store`, persists      | No                    |
| `kevy://host[:port][/db]`          | TCP kevy server            | One socket per open; server-side fan-out          | **Yes**               |
| `redis://host[:port][/db]`         | TCP — alias of `kevy://`   | same                                              | **Yes**               |
| `tcp://host[:port]`                | TCP — raw, no leading `SELECT` | same                                          | **Yes**               |

Anonymous `mem://` cannot receive published messages — nothing else
can reach the same backing `Store`, so `Subscriber::open` rejects
it with `ErrorKind::Unsupported`. Use `mem://<some-name>` whenever
you intend to publish.

`rediss://`, `kevys://`, and `redis://user:pass@…` are rejected for
the same reason: kevy ships without TLS or `AUTH`. Front the socket
with stunnel + IP allowlist at the network boundary if you need
either.

The `mem://<name>` and `file:///` registries are **per-process**:
two unrelated OS processes that open the same name see two
independent buses. Cross-process delivery means running a kevy
server and opening `kevy://host:port` from both sides.

## Trade-offs and limits

- **At-most-once delivery.** A subscriber that disconnects mid-frame
  loses that frame. There is no per-subscriber durable cursor and no
  redelivery. If a frame matters, persist it in a list or stream and
  use pub/sub only as the "wake up" signal.
- **No offline backlog.** A publish that finds zero subscribers
  returns `0` and the body is discarded. There is no buffer that
  catches a subscriber up on what it missed while disconnected.
- **Subscriber back-pressure is per-subscriber, not global.** Each
  subscriber owns its own bounded queue. A slow consumer fills its
  own queue and then drops frames or, on TCP, gets closed by the
  server's client-output-buffer policy. The publish path drops the
  bus mutex before sending, so one slow listener cannot stall
  publishes on unrelated channels — but it also cannot exert
  back-pressure on the publisher.
- **Linux `writev` cap.** On Linux, `writev` hands the kernel at most
  `IOV_MAX = 1024` iovec entries per call. The server batches the
  per-subscriber frame headers and the shared body Arc into iovecs;
  for fan-outs wider than ~340 subscribers per channel (each takes
  three iovec slots) the server splits into multiple `writev` calls
  automatically. The cap shows up only as a soft performance
  ceiling, never as a delivery failure.
- **Subscribed clients are restricted.** A `Subscriber` connection
  rejects non-pub/sub commands; that is why `kevy-client` exposes
  publisher and subscriber as **two separate types** sharing the
  same URL.

## Operational introspection

The standard `PUBSUB` admin subcommand works on both the TCP server
and the URL facade — open a normal `Connection`, not a `Subscriber`,
to call them.

| Subcommand              | Returns                                                                        |
|-------------------------|--------------------------------------------------------------------------------|
| `PUBSUB CHANNELS [pat]` | Array of channels with at least one subscriber, optionally glob-filtered.      |
| `PUBSUB NUMSUB [ch …]`  | Interleaved `channel, count` pairs for each named channel (0 if absent).       |
| `PUBSUB NUMPAT`         | Integer: number of distinct `PSUBSCRIBE` patterns registered, across clients.  |

```sh
$ redis-cli -p 6379 PUBSUB CHANNELS '*'
1) "news"
2) "jobs"
$ redis-cli -p 6379 PUBSUB NUMSUB news jobs missing
1) "news"
2) (integer) 3
3) "jobs"
4) (integer) 1
5) "missing"
6) (integer) 0
$ redis-cli -p 6379 PUBSUB NUMPAT
(integer) 2
```

All three are O(channels) or O(args) point lookups against the
per-shard pub/sub registry; safe to poll from monitoring agents.

## FAQ

**Will a message arrive if the subscriber connected after the
publish?**  No. Pub/sub has no replay. The subscriber index is
consulted at publish time; later subscribers see only frames
published *after* their subscribe ack lands.

**Does `PUBLISH` block the publisher until subscribers drain?**  No.
The publisher's `publish` call returns once the body has been
queued onto every matching subscriber's per-subscriber queue (and,
for TCP subscribers, scheduled onto their socket's write queue). A
slow subscriber holds up its own queue, not yours.

**Can I share one `Subscriber` between async tasks?**  Yes — wrap it
in an `Arc` and `spawn_blocking` the `recv` call. The receive mutex
serialises blocking waits, so each frame is delivered to **exactly
one** task. For real broadcast fan-out (every task sees every
frame), open one `Subscriber` per task — they are cheap. See
[`docs/async.md`](https://github.com/goliajp/kevy/blob/develop/docs/async.md)
for the full async pattern.

**Why does my test see the subscribe ack before any messages?**  The
bus is ordered, but every `SUBSCRIBE` / `PSUBSCRIBE` enqueues an ack
frame *before* the first body frame for that channel arrives. Drain
the ack with one `sub.recv()?` before asserting on payloads — this
matches the redis-cli wire shape.

**Do I need cluster routing for pub/sub?**  No. Pub/sub fan-out is
process-level, not slot-routed: publishing on any shard's port
reaches every subscriber on every shard's port in the same process.
A plain `Connection::open("kevy://host:port")` against any shard
port works. See
[`docs/cluster.md`](https://github.com/goliajp/kevy/blob/develop/docs/cluster.md)
for the slot routing that *keyspace* commands use.
