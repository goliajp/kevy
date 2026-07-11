# API-FREEZE — 4.0 pub-face diff (K-108, the one break window)

Complete inventory of every public-API change made in the 4.0 break
window. Each row: what changed + a one-line migration hint. These
surfaces are **frozen** after this train — later 4.x tasks must not
touch them. Raw material for the T6 UPGRADING guide.

## 1. Removed deprecated `flush` shims

| Crate | Removed | Migration |
|---|---|---|
| kevy-embedded | `Store::flush()` (`#[deprecated]` since 1.2.0) | call `Store::flushall()` — same behavior (WIPES the store) |
| kevy-client | `Connection::flush()` (`#[deprecated]` since 1.8.0) | call `Connection::flushall()` |
| kevy-store | `Store::flush()` (`#[deprecated]` since 1.17.0) | call `Store::flushall()` |

## 2. Error convergence — `KevyError`

New type: `kevy_store::KevyError` + alias `kevy_store::KevyResult<T>`,
re-exported from `kevy_embedded` and `kevy_client`.

Variants: `Store(StoreError)` / `Io(io::Error)` / `Protocol(String)` /
`ReadOnly` / `InvalidInput(String)` / `NotFound(String)` /
`Unsupported(String)` / `TimedOut` / `Closed`. Implements
`Display` + `std::error::Error` + `From<StoreError>` + `From<io::Error>`.

**Placement rationale**: kevy-store sits at the bottom of every
consumer's dependency graph (embedded / client / persist / rt / server
all already depend on it) and is the source of the dominant structured
error (`StoreError`) — hosting the unified type there adds zero new
dependency edges. The alternative (kevy-resp) would have required a
protocol-stone → store edge, inverting layering.

| Change | Migration |
|---|---|
| Every `pub fn` of `kevy_embedded::Store` (and `AtomicCtx` / `AtomicAllShards` / `Pipeline` / `Subscription` facades): `io::Result<T>` → `KevyResult<T>` | change return-type annotations; `?` still works (`From<io::Error>`); match `KevyError::Store(_)` instead of parsing `io::Error` strings |
| Every `pub fn` of `kevy_client::Connection` / `Subscriber` / `ClusterClient` / `Transaction` / `PipelineBuf` / feed & index wraps: `io::Result<T>` → `KevyResult<T>` | same as above; server error replies arrive as `KevyError::Protocol(text)` with the wire text preserved |
| `io::Error::other(format!("kevy-store: {e:?}"))` lossy downgrade (embedded `store_err` / client `store_err`) | eliminated — structured `KevyError::Store(StoreError)` |
| Embedded replica write rejection: `io::Error::other("READONLY …")` | now `KevyError::ReadOnly` (its `Display` still starts with `READONLY` for string-matchers) |
| Embedded pubsub `Subscription::recv*`: `ErrorKind::TimedOut` / `ErrorKind::UnexpectedEof("bus closed")` | now `KevyError::TimedOut` / `KevyError::Closed` |
| Client `Subscriber` stream-gone signal (`ErrorKind::UnexpectedEof`) — also the iterator termination condition of `SubscriberEvents` / `SubscriberMessages` | now `KevyError::Closed`; iterator `Item` type is `KevyResult<_>` |
| Client remote-only feature gate (`ErrorKind::Unsupported`) | now `KevyError::Unsupported(msg)` |
| URL parse errors (`ErrorKind::InvalidInput` / `Unsupported`) | now `KevyError::InvalidInput(_)` / `KevyError::Unsupported(_)` |
| kevy-resp: new `CmdError` enum (`Wire(&'static str)` + `as_wire()` / `Display` / `Error` / `From<&'static str>`) | internal parse/dispatch faces of kevy + kevy-rt no longer use bare `&'static str` as an error type; the wire text is unchanged |
| kevy-rt / kevy: zero `Result<_, &'static str>` remain (was ~48 internal faces: dispatch_geo, dispatch_stream, index/view runtime, exec_build, exec_feed, replication retarget) | internal; no external migration |

Note: there is deliberately **no** `From<KevyError> for io::Error` —
that would reintroduce the silent lossy downgrade this change removes.

## 3. Constructor naming — resources `open`, network `connect`

| Crate | Old | New | Migration |
|---|---|---|---|
| kevy-client | `Connection::open(url)` | `Connection::connect(url)` | rename call |
| kevy-client | `Subscriber::open(url, channels)` | `Subscriber::connect_channels(url, channels)` | rename call (`Subscriber::connect(url)` unchanged) |
| kevy-resp-client | `RespClient::from_url(url)` | `RespClient::connect_url(url)` | rename call (`RespClient::connect(host, port)` unchanged) |
| kevy-client-async | `AsyncConnection::open(url)` | `AsyncConnection::connect(url)` | rename call |
| kevy-client-async | `AsyncSubscriber::open(url, channels)` | `AsyncSubscriber::connect_channels(url, channels)` | rename call |

Unchanged (already conforming): `kevy_embedded::Store::open` (resource),
`kevy_persist::Aof::open` (resource), `kevy_store::Store::new` (pure
in-memory value), `ClusterClient::connect`, `RwClient::connect`.

## 4. `kevy_rt::Runtime` builder

| Old | New |
|---|---|
| `Runtime::new(ip, port, nshards, commands)` | `Runtime::builder(commands).bind(ip, port).shards(n)` |

`Runtime` is its own builder (as before with the `with_*` chain); the
positional 4-arg constructor is gone. `builder(commands)` defaults:
bind `127.0.0.1:6004`, 1 shard, AOF on (`EverySec`), data dir `"."`.
`bind` / `shards` are `#[must_use]` setters like every `with_*`.

## 5. kevy-store write face takes borrowed argv

Owned-argument forms removed; the `_borrowed` names (v1.25 G4) are now
the only — and canonical — names:

| Removed (owned) | Now (same name, borrowed params) |
|---|---|
| `del(&[Vec<u8>])` / `exists(&[Vec<u8>])` | `del(&[&[u8]])` / `exists(&[&[u8]])` |
| `hset(&[(Vec<u8>, Vec<u8>)])` / `hdel(&[Vec<u8>])` / `hmget(&[Vec<u8>])` | `hset(&[(&[u8], &[u8])])` / `hdel(&[&[u8]])` / `hmget(&[&[u8]])` |
| `sadd(&[Vec<u8>])` / `srem(&[Vec<u8>])` | `sadd(&[&[u8]])` / `srem(&[&[u8]])` |
| `lpush(&[Vec<u8>])` / `rpush(&[Vec<u8>])` | `lpush(&[&[u8]])` / `rpush(&[&[u8]])` |
| `zadd(&[(f64, Vec<u8>)])` / `zrem(&[Vec<u8>])` | `zadd(&[(f64, &[u8])])` / `zrem(&[&[u8]])` |
| `zadd_flags_borrowed(...)` | renamed `zadd_flags(...)` |

Migration: pass slices (`b"m".as_slice()`, `vec.as_slice()`) instead of
owned `Vec<u8>`. kevy-embedded's facades now hand their borrowed argv
straight through (the per-call `to_vec()` copies on every write path
are gone).

## 6. kevy-embedded Config filename knobs removed

| Removed | Migration |
|---|---|
| `Config::with_snapshot_filename` + pub field `snapshot_filename` | none — layout is fixed at `dump-{i}.rdb` |
| `Config::with_aof_filename` + pub field `aof_filename` | none — layout is fixed at `aof-{i}.aof` |

The legacy single-file dir (default names `dump-0.rdb` / `aof-0.aof`)
coincides with shard 0's fixed layout, so old default-named dirs load
unchanged; only custom-named dirs (the removed knob) lose support.
`shards.meta` is now always recorded (n == 1 included).

## 7. `min_replicas_max_lag_ms` made real

- New `kevy_rt::ReplicaAck { acked_offset, ack_age_ms }`.
- `Commands::on_replication_view` replicas entry:
  `(Ipv4Addr, u16, u64, Option<u64>)` →
  `(Ipv4Addr, u16, u64, Option<ReplicaAck>)`.
- The min-replicas write gate now counts a replica as healthy only if
  it has ACKed **and** that ACK is within `min_replicas_max_lag_ms`
  (config key existed and is now enforced; stalled replicas no longer
  satisfy `min-replicas-to-write`).

Migration (embedders implementing `Commands`): destructure the
`ReplicaAck` instead of a bare acked offset.

## 8. Everything intentionally NOT changed

- RESP command aliases (`SLAVEOF`, `HMSET`, …) and on-disk legacy read
  paths: kept (protocol/data compat, not API debt).
- `kevy-resp-client`'s `io::Result` face: kept — it is a pure
  transport+RESP stone; `io::Error` is its honest error currency.
- Internal genuine-IO helpers (embedded `listener` / `shard` loaders,
  etc.) keep `io::Result` (private faces; convert at the pub boundary).

## 9. Commands trait clean-up + Route parameterization (K-110, same 4.0 window)

Deep-review verdict follow-up, still inside the one break window.

| Change | Migration |
|---|---|
| `Commands::dispatch_resp3` removed (Vec-returning form; zero callers — the runtime only ever calls `dispatch_into_resp3`) | override `dispatch_into_resp3` for RESP3 shapes |
| `Commands::wake_idx` removed (dead method + silent-failure trap: the runtime reads `ResolvedCmd::wake_idx`, and the default `resolve()` hardcodes `None` without calling the method) | populate `ResolvedCmd::wake_idx` in your `resolve()` override; the field is unchanged |
| `Commands::extension_reduce_v3` + `extension_reduce(argv, chunks) -> Vec<u8>` merged into one method: `extension_reduce(argv, chunks, proto) -> ExtensionReduced` | return `ExtensionReduced::Reply(bytes)`; proto-aware reduces branch on the new `proto` param |
| The `0x00`-prefixed in-band continuation convention (a reduce reply starting with NUL re-fanned an embedded length-prefixed argv) replaced by the explicit `kevy_rt::ExtensionReduced::{Reply, Continue}` enum | return `ExtensionReduced::Continue(argv2)` instead of hand-encoding the NUL frame |
| `Route::{MGet, SInter, SUnion, SDiff, ZInterCard}` collapsed into `Route::Gather(MultiOp)`; `kevy_rt::MultiOp` (5 variants, `ZInterCard` now unit — the LIMIT cap is parsed by the runtime's gather builder) is newly pub | construct `Route::Gather(MultiOp::…)` in `route()`/`resolve()` |
| `Route::{Keys, Scan, RandomKey}` collapsed into `Route::Keyspace(KeyShape, Option<Vec<u8>>)` (pattern carried as before); `kevy_rt::KeyShape` is newly pub | construct `Route::Keyspace(KeyShape::…, pattern)` |
| `Route`, `XGroupCtx`, `SlowlogSub` now derive `PartialEq` (`MultiOp` / `KeyShape` add `PartialEq + Eq`) | none (additive) |

Dead dependency edges removed (declared, zero source usage):
kevy-lua → kevy-resp, kevy-lua → kevy-bytes, kevy-chaos → kevy-resp-client.

Behavior note (kevy server, not a wire break): `KevyCommands::route()`
now delegates to the resolve-side table (single source of truth,
parity-tested over the whole verb registry). Truth-direction fixes
folded in: BLPOP/BRPOP/BZPOPMIN/BRPOPLPUSH route `Local` (park on the
conn's origin shard), REPLICAOF/SLAVEOF/ROLE route `Local` on the hot
path (previously `Single(1)` via resolve's catch-all), keyless
XGROUP/XINFO forms route `Local`.
