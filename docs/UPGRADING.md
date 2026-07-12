# Upgrading kevy

Two chapters, newest first: **3.x → 4.0** (an API-definition major:
the wire and the disk carry over, the Rust faces changed once and are
now frozen) and **2.x → 3.x** (a capability major: everything carried
over). Each chapter is explicit about what upgrades automatically,
what needs a code change, and how to go back.

---

# 3.x → 4.0

4.0 is the industrial-production declaration. The semver major was
spent deliberately — one break window in which every long-standing
public-API debt was paid, and after which the surfaces are **frozen**:
later 4.x releases are additive only. If you talk to kevy over the
wire, 4.0 is a binary swap. If you link a kevy crate, expect a short,
mechanical migration — every change below has a one-line rule.

## TL;DR — versions at a glance

| Component | 3.x era | 4.0 | Action |
|---|---|---|---|
| `kevy` (server) | 3.18.x | 4.0.0 | swap the binary, restart on the same data dir |
| `kevy-embedded` | 3.18.x | 4.0.0 | bump + apply the API table below |
| `kevy-client` | **1.14.x** | **4.0.0** | bump — version-line unification (same move `kevy-embedded` made at 3.0.0) + the API table |
| `kevy-client-async` | **1.1.x** | **4.0.0** | same |
| `kevy-wasm` / `@goliajp/kevy` (npm) | — | 4.0.0 | new in 4.0 — [docs/wasm.md](wasm.md) |
| Infra crates (`kevy-store`, `kevy-rt`, …) | 3.18.x | 4.0.0 | follow the workspace version |

## What is compatible automatically

**Wire protocol.** RESP is unchanged, and every verb alias (`SLAVEOF`,
`HMSET`, …) is kept. Redis clients, scripts, and `redis-cli` sessions
work as before; the reply-parity suite against valkey 9.1 still gates
CI.

**Snapshots and AOF.** A 4.0 binary reads every 3.x (and 2.x)
snapshot format and replays 3.x AOFs unchanged. Formats did not
change in 4.0 — downgrade to 3.18 is equally a binary swap (unlike
the 3.x → 2.x direction, which has a format edge; see the 2.x → 3.x
chapter).

**Config.** Every 3.x config key is accepted and means the same
thing. Two keys got *stricter or truer* semantics — see "Behavior
changes" below (`notify_keyspace_events` unknown-flag rejection,
`min_replicas_max_lag_ms` enforcement).

**One removed knob.** Custom snapshot/AOF *filenames* are gone (the
`kevy-embedded` `Config::with_snapshot_filename` /
`with_aof_filename` builders). The on-disk layout is now fixed at
`dump-{i}.rdb` / `aof-{i}.aof` per shard. Directories that used the
default names — including every legacy single-file dir — load
unchanged; only dirs written under a *custom* name need a one-time
`mv` to the fixed names before the first 4.0 open.

## The API break table

Everything in this section is a compile-time break with a mechanical
fix. Nothing here changes runtime semantics except where called out.

### 1. `flush()` shims removed

The deprecated aliases are gone; the surviving name says what it does
(it WIPES the store):

| Crate | Removed | Call instead |
|---|---|---|
| `kevy-embedded` | `Store::flush()` | `Store::flushall()` |
| `kevy-client` | `Connection::flush()` | `Connection::flushall()` |
| `kevy-store` | `Store::flush()` | `Store::flushall()` |

### 2. One error currency: `KevyError`

Every public fallible face of `kevy-embedded` and `kevy-client` now
returns `KevyResult<T>` (`Result<T, KevyError>`) instead of
`io::Result<T>`. The type lives in `kevy-store` and is re-exported
by both crates:

**One deliberate exception.** The change feed keeps its own error type:
`changes_since` and `changes_tail` return `Result<_, FeedError>`, and there is
no `From<FeedError> for KevyError`. `FeedError::Resync` and `FeedError::Future`
are not failures — they are stream control signals telling the reader where it
is relative to the log, and folding them into a general-purpose error enum would
invite callers to `?` past the one thing they must handle. See
[CDC feeds](cdc.md).

```rust
pub enum KevyError {
    Store(StoreError),      // structured engine errors, no longer
                            // flattened into io::Error strings
    Io(std::io::Error),     // real I/O, preserved via From
    Protocol(String),       // server error replies, wire text intact
    ReadOnly,               // replica write rejection
    InvalidInput(String),   // e.g. URL parse errors
    NotFound(String),
    Unsupported(String),    // e.g. remote-only calls on embedded
    TimedOut,               // e.g. Subscription::recv_timeout
    Closed,                 // stream/bus gone; also terminates
                            // subscriber iterators
}
```

Migration is usually just the return-type annotation — `?` keeps
working because `From<io::Error>` and `From<StoreError>` exist:

```rust
// 3.x
fn warm(conn: &mut Connection) -> std::io::Result<()> {
    conn.set(b"greeting", b"hello")?;
    Ok(())
}

// 4.0
use kevy_client::{Connection, KevyResult};

fn warm(conn: &mut Connection) -> KevyResult<()> {
    conn.set(b"greeting", b"hello")?;
    Ok(())
}
```

Code that *inspected* errors gets strictly better off — match the
variant instead of parsing strings:

| 3.x signal | 4.0 |
|---|---|
| `io::Error::other("kevy-store: …")` wrapper text | `KevyError::Store(e)` — the structured `StoreError` |
| replica write rejection: `io::Error::other("READONLY …")` | `KevyError::ReadOnly` (its `Display` still starts with `READONLY`) |
| server `-ERR …` replies as opaque `io::Error` | `KevyError::Protocol(text)` — wire text preserved |
| `ErrorKind::TimedOut` from `Subscription::recv_timeout` | `KevyError::TimedOut` |
| `ErrorKind::UnexpectedEof` as the subscriber-stream-gone signal | `KevyError::Closed`; `SubscriberEvents` / `SubscriberMessages` iterators yield `KevyResult<_>` and end on it |
| `ErrorKind::Unsupported` for remote-only features | `KevyError::Unsupported(msg)` |

There is deliberately **no** `From<KevyError> for io::Error`: that
back-edge would reinstate the lossy downgrade this change removes. At
a boundary that truly needs `io::Error`, convert explicitly and own
the loss.

(`kevy-resp-client` keeps its `io::Result` face on purpose — it is a
pure transport stone and `io::Error` is its honest currency.)

### 3. Constructor naming: resources `open`, network `connect`

One verb per kind, everywhere. Local, file-backed things `open`;
things with a peer `connect`; pure in-memory values `new`. The
renames:

| Crate | 3.x | 4.0 |
|---|---|---|
| `kevy-client` | `Connection::open(url)` | `Connection::connect(url)` |
| `kevy-client` | `Subscriber::open(url, channels)` | `Subscriber::connect_channels(url, channels)` |
| `kevy-client-async` | `AsyncConnection::open(url)` | `AsyncConnection::connect(url)` |
| `kevy-client-async` | `AsyncSubscriber::open(url, channels)` | `AsyncSubscriber::connect_channels(url, channels)` |
| `kevy-resp-client` | `RespClient::from_url(url)` | `RespClient::connect_url(url)` |

Already conforming, unchanged: `kevy_embedded::Store::open`,
`kevy_persist::Aof::open`, `kevy_store::Store::new`,
`ClusterClient::connect`, `RwClient::connect`,
`Subscriber::connect(url)`, `RespClient::connect(host, port)`.

### 4. `kevy_rt::Runtime` is built, not positioned

The positional constructor is gone; `Runtime` is its own builder:

```rust
// 3.x
let rt = Runtime::new([127, 0, 0, 1], 6004, 4, commands);

// 4.0
let rt = Runtime::builder(commands)
    .bind([127, 0, 0, 1], 6004)
    .shards(4);
```

`builder(commands)` defaults: bind `127.0.0.1:6004`, 1 shard, AOF on
(`EverySec`), data dir `"."`. `bind` / `shards` are `#[must_use]`
setters like the existing `with_*` chain. This is also the visible
face of the 4.0 instance work: a `Runtime` no longer touches global
state, so one process can run several independent kevy instances.

### 5. `kevy-store` writes take borrowed argv

The owned-argument forms are removed; the borrowed forms (previously
the `_borrowed` twins) now own the canonical names:

| 3.x (owned) | 4.0 (borrowed, same name) |
|---|---|
| `del(&[Vec<u8>])` / `exists(&[Vec<u8>])` | `del(&[&[u8]])` / `exists(&[&[u8]])` |
| `hset(&[(Vec<u8>, Vec<u8>)])` / `hdel(&[Vec<u8>])` / `hmget(&[Vec<u8>])` | `hset(&[(&[u8], &[u8])])` / `hdel(&[&[u8]])` / `hmget(&[&[u8]])` |
| `sadd` / `srem` / `lpush` / `rpush` / `zrem` `(&[Vec<u8>])` | same names, `(&[&[u8]])` |
| `zadd(&[(f64, Vec<u8>)])` | `zadd(&[(f64, &[u8])])` |
| `zadd_flags_borrowed(…)` | renamed `zadd_flags(…)` |

```rust
// 3.x
store.del(&[b"k1".to_vec(), b"k2".to_vec()]);

// 4.0 — pass slices; no allocation
store.del(&[b"k1".as_slice(), b"k2".as_slice()]);
```

This is a performance fix wearing an API change: `kevy-embedded`'s
facades now hand borrowed argv straight through, and the per-call
`to_vec()` copies on every embedded write path are gone.

### 6. `Commands` trait + `Route` (embedders with a custom command set)

Only relevant if you `impl kevy_rt::Commands` yourself:

- `dispatch_resp3` (Vec-returning form) removed — override
  `dispatch_into_resp3`.
- `wake_idx` method removed — populate `ResolvedCmd::wake_idx` in
  your `resolve()`; the field is unchanged.
- `extension_reduce_v3` and the old two-arg `extension_reduce` merged
  into `extension_reduce(argv, chunks, proto) -> ExtensionReduced`;
  return `ExtensionReduced::Reply(bytes)`, or
  `ExtensionReduced::Continue(argv2)` instead of the old
  NUL-prefixed in-band continuation frame.
- `Route::{MGet, SInter, SUnion, SDiff, ZInterCard}` collapsed into
  `Route::Gather(MultiOp)`; `Route::{Keys, Scan, RandomKey}` into
  `Route::Keyspace(KeyShape, Option<Vec<u8>>)`. `MultiOp` and
  `KeyShape` are newly public.
- `Commands::on_replication_view` replicas entry is now
  `(String, Ipv4Addr, u16, u64, Option<ReplicaAck>)` — a leading
  replica-id `String`, then the peer `(Ipv4Addr, u16)`, the sent
  offset, and `ReplicaAck { acked_offset, ack_age_ms }` (which
  replaces the bare acked offset); destructure it.

## Behavior changes (no code change, ops-visible)

- **`-LOADING` is real.** While a replica swallows a full-resync
  snapshot, reads answer `-LOADING` instead of serving the
  half-replaced dataset. `PING`, `INFO`, and `HELLO` stay answerable
  (health checks keep working), matching the verbs Redis exempts.
  Retry-on-`-LOADING` loops written for Redis behave correctly as-is.
- **`notify_keyspace_events` rejects unknown flag characters** at
  config parse instead of silently ignoring them — and the flag set
  grew real `x` (expired), `e` (evicted), and `n` (new-key) events.
  A config that previously smuggled a typo through will now fail
  loudly; fix the flag string.
- **`min-replicas-to-write` counts only live ACKs.** The
  `min_replicas_max_lag_ms` key existed in 3.x and is now enforced: a
  replica whose last ACK is older than the window no longer satisfies
  the write gate. Deployments relying on a *stalled* replica to keep
  writes flowing will see `-NOREPLICAS` — which is the semantics the
  key always promised.
- **The `CLIENT` face tells the truth**: `CLIENT LIST` is the real
  connection table (getpeername-backed addresses, globally unique
  ids), `CLIENT KILL` really kills (including blocked connections),
  `CLIENT SETNAME` sticks, and `connected_clients` in `INFO` is a
  live gauge.
- **`SHUTDOWN` drains gracefully**: in-flight replies finish and the
  AOF gets a final fsync before exit, closing the everysec tail
  window that a bare SIGTERM used to leave unsynced.
- **`ROLE` / `INFO replication` aggregate across shards** with
  per-replica identity (`ip:port` and true per-replica offsets).
  Parsers that assumed one summary line per server keep working; the
  per-replica lines are richer.

## The feature system (new in 4.0)

`kevy-embedded` is now feature-tiered so small targets pay only for
what they use. The default remains everything-on:

| Feature | Adds | Pulls in |
|---|---|---|
| `core` | KV + TTL + pubsub + atomic/pipeline | (nothing) |
| `persist` | snapshots + AOF | `kevy-persist` |
| `index` | declared indexes + views | `kevy-index` |
| `text` | full-text (BM25) segments | `index`, `kevy-text` |
| `vector` | HNSW ANN segments | `index`, `kevy-vector` |
| `replicate` | replication + CDC feed | `persist`, `kevy-replicate` |
| `listener` | read-only RESP listener | (nothing) |

The `core` tier cross-compiles for musl targets and holds an
enforced budget (≤ 700 KB binary, ≤ 2 MB empty-store RSS); five
foundation crates additionally build `no_std`. See
[docs/iot.md](iot.md). At the other end of the size spectrum, the
same embedded core now runs in the browser as `@goliajp/kevy` —
see [docs/wasm.md](wasm.md).

## Downgrading 4.0 → 3.18

Binary swap back; snapshot and AOF formats are shared. The only edge:
a config file using the new notify flags (`x`/`e`/`n`) parses on 3.18
but the events never fire there.

---

# 2.x → 3.x

kevy 3.x is a superset of 2.x: every 2.x workload runs unchanged, and
the upgrade is a binary swap for the server and a dependency bump for
embedded users. This chapter is explicit about what carries over
automatically, what changed names or numbers, and the one direction
that needs care (downgrading back to 2.x).

## TL;DR — versions at a glance

| Component | 2.x era | 3.x (final: 3.18.x) | Action |
|---|---|---|---|
| `kevy` (server) | 2.0.x | 3.18.x | swap the binary, restart on the same data dir |
| `kevy-embedded` | **1.x** (1.4–1.16) | **3.x** | bump the dep — the 1.x line ended at v3.0.0 when the whole workspace unified on one version |
| `kevy-client` | 1.12.x | 1.13–1.14 | bump; API unchanged |
| `kevy-client-async` | 1.0.x | 1.1.x | bump; API unchanged |
| `kevy-cli` | unpublished | 3.x | `cargo install kevy-cli` — now carries the whole migration toolchain |
| Infra crates (`kevy-store`, `kevy-rt`, …) | 2.0.x | 3.x | follow the workspace version |

The `kevy-embedded` jump from 1.x to 3.x is a **version-line
unification, not an API rewrite**: the 1.16 surface is contained in
3.x. If your `Cargo.toml` says `kevy-embedded = "1"`, change it to
`"3"` and rebuild — or go straight to `"4"` with the chapter above.

## What is compatible automatically

**Wire protocol.** RESP is unchanged. 3.x remains reply-checked
byte-for-byte against valkey 9.1 in CI (98 commands). Existing Redis
clients, scripts, and `redis-cli` sessions work as before.

**Snapshots.** The 3.x loader reads every 2.x snapshot format
(`KEVYSNAP` versions 2–5): relative-TTL v2 files, absolute-TTL v3,
stream-group v4, and feed-cursor v5. Point a 3.x server at a 2.x data
directory and it loads.

**AOF.** The AOF is a verb log and 3.x's verb set is a superset of
2.x's — replay works unchanged. `appendfsync` semantics are
unchanged.

**Config.** Every 2.x config key is accepted. New sections
(`[replication] single_source`, `--accept-shards`, …) are additive
with defaults that reproduce 2.x behavior.

## Upgrade steps

### Server deployment

1. Take a snapshot on the running 2.x server (`SAVE` or your normal
   backup), and keep a copy — see “Downgrading” below for why.
2. Stop 2.x, start the 3.x binary with the same flags and data dir.
3. Verify: `DBSIZE` matches, and if you want cryptographic-grade
   assurance run `kevy-cli digest -p <port> <prefix>` before and
   after — equal digests mean an identical keyspace.

Rolling a replica pair: upgrade the replica first, let it re-sync,
then fail traffic over and upgrade the former primary. (2.x has no
managed failover, so from 2.x this is the usual manual swap. Once
you are on 3.15+, the fail-over step itself becomes one verb —
`FAILOVER host port` — see
[docs/availability.md](availability.md).)

### Embedded applications

1. `kevy-embedded = "3"` in `Cargo.toml`.
2. Rebuild. The 1.16 API is present unchanged; new capability
   surfaces (index/view/text/vector/feed/replication) are additive
   methods and `Config` options.
3. One trait note: if (and only if) you wrote a custom
   `impl kevy_rt::Commands` and construct `ResolvedCmd` literals,
   two fields were added during the v2 arc (`block_hint`,
   `wake_idx`). The default `resolve()` fills them; literal
   constructors add the two fields.
4. On-disk data from an embedded 1.x app loads as-is (same snapshot
   formats as the server).

### Clients

`kevy-client 1.13+` / `kevy-client-async 1.1` are drop-in: the minor
bump only re-pins internal crates to the 3.x workspace. Generic Redis
client libraries are unaffected either way.

## What 3.x adds (why you upgrade)

Declared indexes with hydration (`IDX.*`), named views (`VIEW.*`),
write-time aggregates (GROUP BY / distributed top-K), dictionary-free
CJK full-text search with BM25, HNSW vector KNN (plus hybrid BM25+KNN
fusion), CDC feeds with the recovery-point contract (`FEED.*`),
embedded-as-primary replication, the machine-readable contract
(`COMMAND DOCS`, generated references, the `kevy-mcp` MCP server),
the availability arc (replication lag truth, `FAILOVER`, quorum crash
elections, the `WAIT` / `REPL.TOKEN` / `REPL.WAIT` consistency
ladder — [docs/availability.md](availability.md)), and the migration
toolchain (`kevy-cli import/export/--verify/diff/
inspect/digest`). Start at [docs/designing-on-kevy.md](designing-on-kevy.md)
and [docs/cookbook.md](cookbook.md); performance receipts live in
[bench/PERF-LEDGER.md](../bench/PERF-LEDGER.md).

None of these activate implicitly: a 3.x server with a 2.x workload
has an empty catalog, and the index hook on an empty catalog is on
the perfgate ratchet (no regression vs 2.x).

## Downgrading (the one direction that needs care)

A 3.x server **writes** snapshot format v4, or v5 once a CDC feed
cursor exists. A 2.x binary reads at most v4:

- If you never enabled feeds, a 3.x snapshot loads on 2.x.
- If feeds were active (v5), 2.x refuses the file. Downgrade path:
  `kevy-cli export` on 3.x → `kevy-cli import` into a fresh 2.x —
  or restore the pre-upgrade backup from step 1 and accept the gap.

Verbs introduced in 3.x (`IDX.*`, `VIEW.*`, `FEED.*`, …) naturally
don't replay on a 2.x binary — if you used them, the export/import
path is the correct downgrade, not AOF replay.

## Version history in one line each

- **3.0.0** — the serving-engine declaration (indexes, views, FTS,
  ANN, CDC, on-ramp; eleven gated trains).
- **3.8.0** — the perf arc (measured vs valkey 9.1 and RediSearch;
  bare face 1.6–3.3×, ANN 1.64× ahead at recall 1.000, FTS single
  common term 93×; embedded-as-primary replication). No releases
  were cut between 3.0.0 and 3.8.0; 3.8.0 contains trains v3.1–v3.8.
- **3.17.0** — the availability release: the AI-native serving faces
  (machine-readable verb contract, generated docs, `kevy-mcp`, hybrid
  retrieval) and the availability arc (replication heartbeat/ACK
  truth, `FAILOVER` + quorum crash elections, the consistency
  ladder, contract gates in CI). Contains trains v3.9–v3.17; no
  releases were cut in between.
- **3.17.1–3.17.4** — maintenance: the `luna-core` Lua runtime bump,
  the docs/migration wave, first-adopter feedback (`kevy-cli
  --embed`), and the docs/i18n polish wave.
- **3.18.0** — the structure release: LOC debt to zero with the
  limits enforced in CI, six more stones fuzzed (day-one harvest:
  four real bugs fixed), miri/pedantic/missing-docs sweeps, Rust
  1.97.0.
