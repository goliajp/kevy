# kevy Client Contract (language-agnostic)

**Status:** canonical spec, extracted from the Rust reference implementation
(`crates/kevy-client`, `crates/kevy-client-async`, `crates/kevy-embedded`,
`crates/kevy-ffi`, `crates/kevy-resp`) plus `docs/verb-reference.md`.

**Audience:** anyone porting a first-party typed kevy client to Go, C/C++,
Java, TypeScript (Node/Bun), Python, or C#. This document is the shared
target for TDD: every port implements the *same* observable contract, and
its test suite exercises the conformance checklist in §6.

**Hard requirement (differs from the Rust reference):** in every non-Rust
port, **one client type exposes BOTH a synchronous and an asynchronous
surface** — not two packages. The Rust reference happens to split them into
`kevy-client` (sync) and `kevy-client-async` (async) for crate-hygiene
reasons, but that split is a Rust artefact. Ports MUST unify: e.g. TS exposes
`get()`/`getSync()` (or an async-by-default client with a sync escape hatch)
from a single package; Python exposes both a blocking client and an
`asyncio` client from one module; Go's methods are blocking with
`context.Context` for cancellation; etc. See §1.4.

---

## 1. Connection model

### 1.1 One URL, two transports

A single `connect(url)` entry point chooses the backend from the URL scheme.
The *same business code* runs against an in-process embedded store or a
remote RESP server by changing only the URL string.

| Scheme | Target | Backend |
|---|---|---|
| `mem://` | anonymous in-memory | **Embedded** — a fresh in-process `Store` each `connect`, never shared |
| `mem://<name>` | named in-memory bus | **Embedded** — shared by `<name>` across the process (same-process pub/sub works) |
| `file:///abs/path` / `file://./rel/path` | persistent | **Embedded** — in-process `Store` with snapshot + AOF persistence, shared by canonical path across the process |
| `kevy://host[:port][/db]` | remote | **Remote** — TCP RESP, kevy-native scheme |
| `redis://host[:port][/db]` | remote | **Remote** — TCP RESP, standard Redis URL (alias for `kevy://`) |
| `tcp://host[:port]` | remote | **Remote** — TCP RESP, raw (no `SELECT` round-trip; ignores any `/db`) |

Rejected up front (before any I/O):
- `rediss://`, `kevys://` → **Unsupported** ("kevy has no TLS").
- `redis://user:pass@host` (userinfo / AUTH) → **Unsupported** ("kevy has no AUTH").
- Any other scheme → **InvalidInput** ("unknown URL scheme").
- `file://` with an empty path → **InvalidInput**.

Default port when omitted: **6379**.

### 1.2 Remote `/db` selection

`kevy://` and `redis://` may carry a `/N` database index. On connect the
client issues one `SELECT N` round-trip; a `-ERR` reply to `SELECT` fails the
connect. `tcp://` never does a `SELECT` (raw). Embedded schemes ignore db
indices (single logical DB).

### 1.3 Process-local embedded registry

Two `connect()` calls with the *same* `mem://<name>` or `file:///path` URL
resolve to the **same** backing `Store` (and the same pub/sub bus). This is
what makes embedded pub/sub work end-to-end: a publisher `Connection` and a
consumer `Subscriber` opened on the same URL find each other. Anonymous
`mem://` (no name) is always isolated — its own private bus by design.

Ports MUST implement this registry as a **process-global, URL-keyed weak map**:
- key = `mem://<name>` or `file://<canonical-path>`;
- value = a weak handle to the open store;
- entries evict when the last strong handle drops (next `connect` for that
  URL gets a fresh store).

### 1.4 Sync + async in one client (port requirement)

The unified client MUST offer, for every command:
- a blocking call, and
- an async call (the language's idiomatic future/promise/coroutine).

Embedded backends are inherently synchronous (in-process mutex); the async
face over an embedded URL may simply run the blocking op (documented as such)
or reject embedded URLs on the async path exactly as the Rust async crate
does (async is TCP-only there). Ports SHOULD support embedded on both faces
for ergonomics, but MUST at minimum support **remote on both faces** and
**embedded on the sync face**.

**Language caveat (validated by the TS port):** a language with no
synchronous socket read — JavaScript/TypeScript on Node and Bun — cannot
offer a synchronous *remote* face at all. There the accepted shape is:
async-by-default for both backends, and a synchronous **embedded-only** face
(a `.sync` surface / `connectSync`) that throws `Unsupported` on a remote
URL. Go/Rust/Java/Python/C#/C++ have blocking sockets and give both faces on
both backends; JS/TS is the documented exception.

The async surface in the Rust reference is TCP-only and covers the
string/generic + hash/list/set/zset + pipeline + cluster + subscriber
families. Ports SHOULD extend async coverage to the full family set (that is
strictly a superset and does not violate the contract).

---

## 2. Error model

### 2.1 Errors as values vs exceptions

The Rust reference is **errors-as-values**: every method returns
`Result<T, KevyError>` (sync) or `io::Result<T>` (async). Ports map this to
their idiom:

| Language | Mapping |
|---|---|
| Go | `(T, error)` — a typed `KevyError` implementing `error`, inspectable via `errors.As` |
| Java | checked/unchecked `KevyException` hierarchy (one subclass per variant) |
| TypeScript | **throw** a `KevyError` subclass (Node/Bun idiom); MAY also expose a `Result` union for hot paths |
| Python | raise a `KevyError` exception hierarchy |
| C# | throw a `KevyException` hierarchy |
| C | return an `int` status code + out-param for the value; negative = misuse, and a protocol `-ERR` is a *successful call* carrying a RESP error frame (see §5) |

The **variant identity** must survive the mapping (a port must let callers
distinguish, say, a wrong-type store error from a transport error), because
the conformance tests assert on variant.

### 2.2 `KevyError` variants (the canonical error taxonomy)

```
KevyError =
  | Store(StoreError)   // structured store-semantic error (see §2.3)
  | Io(<native>)        // OS/transport failure (file, socket, AOF)
  | Protocol(String)    // RESP-level: a server -ERR reply (text preserved verbatim)
                        //   OR a malformed / unexpected reply shape
  | ReadOnly            // write rejected: target is a read-only replica
  | InvalidInput(String)// bad argument to a typed API — rejected before touching state
  | NotFound(String)    // a named object (index, view, key) doesn't exist
  | Unsupported(String) // op not available on this backend/build (e.g. IDX.* on embedded, TLS)
  | TimedOut            // a bounded blocking call ran out its timeout
  | Closed              // the connection / in-process bus is gone (EOF)
```

**Key distinction — store-semantic vs protocol vs transport error:**
- A server error **reply** whose text is a **recognized store-semantic
  error** — `WRONGTYPE …`, `… is not an integer or out of range`, `… is not
  a valid float`, `… out of range`, `no such key`, `OOM …` — surfaces as
  `KevyError::Store(<variant>)` (§2.3), **on both backends**. The remote path
  recognizes these from the reply text; the embedded path returns the same
  structured variant directly. This is what lets the conformance tests (§6)
  assert `Store(WrongType)` uniformly on embedded *and* remote.
- Any **other** server error reply (`-ERR wrong number of arguments`, a
  verb-specific `-ERR …`, etc.) surfaces as `Protocol(<text>)` with the wire
  text preserved verbatim (minus the leading `-`). It is a *successful
  round-trip that returned an error frame*.
- A malformed frame or an unexpected reply *shape* (e.g. an `Int` where a
  `Bulk` was expected) also surfaces as `Protocol(<description>)`.
- A socket/file failure surfaces as `Io`.
- A clean server-side connection close mid-read surfaces as `Closed`
  (Rust async maps this to `UnexpectedEof`).

### 2.3 `StoreError` (structured store-semantic errors)

Carried inside `KevyError::Store`. These stay **structured**, never stringly:

```
StoreError =
  | WrongType    // key holds a different type than the command expects (Redis WRONGTYPE)
  | NotInteger   // value is not a base-10 integer (INCR family)
  | Overflow     // result would overflow i64
  | OutOfRange   // index outside the collection (LSET)
  | NoSuchKey    // key does not exist where the command requires one (LSET)
  | NotFloat     // value is not a valid float (INCRBYFLOAT)
  | OutOfMemory  // maxmemory exceeded under NoEviction (Redis OOM)
```

### 2.4 Async `ErrorKind` mapping (the Rust async contract, reproduced)

The Rust async crate returns `io::Result` and encodes variants as
`io::ErrorKind` with the wider context in the message. Ports that expose a
unified error type SHOULD preserve this correspondence so callers can branch
uniformly across sync and async:

| Source | Kind / variant |
|---|---|
| RESP `-ERR …` reply | `Protocol` (Rust async: `Other`) |
| unexpected reply variant | `Protocol` (Rust async: `Other`) |
| malformed RESP frame | `Protocol` (Rust async: `InvalidData`) |
| server closed connection mid-read | `Closed` (Rust async: `UnexpectedEof`) |
| unknown URL scheme / bad port | `InvalidInput` |
| TLS / AUTH / embed URL on async path | `Unsupported` |
| underlying socket I/O | `Io` (native kind) |

---

## 3. Command families

Notation for return shapes (language-neutral):
`bytes` = raw byte string; `str` = UTF-8 string; `int` = signed 64-bit;
`uint`/`count` = non-negative integer; `bool`; `f64`; `opt<T>` = nullable;
`list<T>`; `map` = flat `[k0,v0,k1,v1,…]` bytes list (Redis wire shape);
`()` = success/void. All key/value/member/field parameters are **`bytes`**
(binary-safe); the client never assumes UTF-8 for user data.

Every method below is exposed on **both** the sync and async face of the
unified client. "Embedded" and "Remote" columns note backend availability;
`Unsupported` means the embedded backend rejects it with
`KevyError::Unsupported` (caller should reach for the embedded `Store` typed
API — §5 — for those).

### 3.1 Core string / generic key

| Method | Params | Returns | Notes |
|---|---|---|---|
| `ping` | — | `()` | `+PONG` expected; embedded always OK |
| `set` | `key, value` | `()` | unconditional SET (no NX/XX) |
| `get` | `key` | `opt<bytes>` | `None` if absent/expired |
| `del` | `keys: list` | `count` | number actually removed |
| `exists` | `keys: list` | `count` | a repeated key counts each time (Redis semantics) |
| `incr` | `key` | `int` | post-increment; `NotInteger` on non-numeric |
| `incr_by` | `key, delta: int` | `int` | negative delta = DECRBY |
| `expire` | `key, ttl: duration` | `bool` | wire `PEXPIRE key <ms>`; whether key existed & got TTL |
| `persist` | `key` | `bool` | whether a TTL was removed |
| `ttl_ms` | `key` | `int` | wire `PTTL`; ms remaining, `-2` no key, `-1` no TTL |
| `type_of` | `key` | `str` | `"string"`/`"hash"`/`"list"`/`"set"`/`"zset"`/`"none"` |
| `dbsize` | — | `count` | live keys at call time |
| `flushall` | — | `()` | wipes the store (named `flushall`, not `flush`) |
| `set_with_ttl` | `key, value, ttl: duration` | `()` | atomic `SET … PX <ms>` |
| `mget` | `keys: list` | `list<opt<bytes>>` | one per key, in order; `None` for missing/wrong-type |
| `mset` | `pairs: list<(key,value)>` | `()` | atomic |
| `publish` | `channel, message` | `count` | subscribers reached (embedded named bus delivers a real count; anonymous `mem://` returns 0) |

Duration→ms conversion is clamped to `i64::MAX` ms.

### 3.2 Hash

| Method | Params | Returns | Notes |
|---|---|---|---|
| `hset` | `key, pairs: list<(field,value)>` | `count` | newly added fields (not overwrites) |
| `hget` | `key, field` | `opt<bytes>` | |
| `hdel` | `key, fields: list` | `count` | |
| `hlen` | `key` | `count` | 0 if absent |
| `hgetall` | `key` | `map` (flat `[f0,v0,…]`) | empty if absent |
| `hkeys` | `key` | `list<bytes>` | |
| `hvals` | `key` | `list<bytes>` | |

### 3.3 List

| Method | Params | Returns | Notes |
|---|---|---|---|
| `lpush` | `key, values: list` | `count` | new length |
| `rpush` | `key, values: list` | `count` | new length |
| `lpop` | `key, count: uint` | `list<bytes>` | up to `count` from head; empty if drained |
| `rpop` | `key, count: uint` | `list<bytes>` | from tail |
| `llen` | `key` | `count` | |
| `lrange` | `key, start: int, stop: int` | `list<bytes>` | Redis negative indexing |

### 3.4 Set

| Method | Params | Returns | Notes |
|---|---|---|---|
| `sadd` | `key, members: list` | `count` | newly added |
| `srem` | `key, members: list` | `count` | removed |
| `smembers` | `key` | `list<bytes>` | order implementation-defined |
| `scard` | `key` | `count` | |
| `sismember` | `key, member` | `bool` | |
| `sinter` | `keys: list` | `list<bytes>` | intersection |
| `sunion` | `keys: list` | `list<bytes>` | union |
| `sdiff` | `keys: list` | `list<bytes>` | first minus the rest |

*Embedded note:* `sinter`/`sunion`/`sdiff` are computed client-side over
`smembers` snapshots (embedded has no server combine). Result order is
unspecified.

### 3.5 Sorted set

| Method | Params | Returns | Notes |
|---|---|---|---|
| `zadd` | `key, pairs: list<(score: f64, member)>` | `count` | newly added |
| `zrem` | `key, members: list` | `count` | removed |
| `zscore` | `key, member` | `opt<f64>` | |
| `zcard` | `key` | `count` | |
| `zrange` | `key, start: int, stop: int` | `list<bytes>` | ascending score; negative indices from tail; members only (no scores) |

### 3.6 Sorted-set algebra (`zalgebra`)

| Method | Params | Returns | Notes |
|---|---|---|---|
| `zinterstore` | `dest, keys: list` | `count` | unweighted, `AGGREGATE SUM`; dest cardinality |
| `zinterstore_with` | `dest, keys, weights: opt<list<f64>>, aggregate: ZAggregate` | `count` | `weights` (if given) must be one-per-key |
| `zunionstore` | `dest, keys: list` | `count` | unweighted SUM |
| `zunionstore_with` | `dest, keys, weights: opt<list<f64>>, aggregate: ZAggregate` | `count` | |
| `zintercard` | `keys: list, limit: opt<uint>` | `count` | cardinality of intersection without materialising; `limit` short-circuits |

`ZAggregate = Sum | Min | Max` (default `Sum`; wire tag emitted only for
non-default). Empty `keys` → `InvalidInput` ("needs at least one source key").
Available on both backends.

### 3.7 Hash-field TTL (Redis 7.4 shape)

| Method | Params | Returns | Notes |
|---|---|---|---|
| `hexpire` | `key, fields: list, ttl: duration, cond: HExpireCond` | `list<HExpireCode>` | **whole-second** precision (ttl truncated to secs) |
| `hpexpire` | `key, fields, ttl: duration, cond` | `list<HExpireCode>` | millisecond precision |
| `hpersist` | `key, fields` | `list<HExpireCode>` | clear per-field TTLs |
| `httl` | `key, fields` | `list<int>` | remaining TTL per field, **seconds** |
| `hpttl` | `key, fields` | `list<int>` | remaining TTL per field, **milliseconds** |

`HExpireCond = Always | Nx | Xx | Gt | Lt` (at most one; wire keyword emitted
for the non-`Always` cases).

`HExpireCode` (signed 8-bit) codes:
- `HEXPIRE`/`HPEXPIRE`: `-2` key/field missing, `0` condition not met,
  `1` deadline set, `2` field deleted (deadline already due).
- `HPERSIST`: `-2` missing, `-1` had no TTL, `1` cleared.
- `HTTL`/`HPTTL`: `-2` key/field missing, `-1` no TTL, else remaining time.

Replies come back in request order. Empty `fields` → `InvalidInput`.
Available on both backends.

### 3.8 Declarative secondary indexes (`IDX.*`) — **Remote-only**

The embedded backend answers `Unsupported` for all `idx_*` methods (the wire
face coerces query bounds through the index's declared type server-side,
which the client cannot replicate without the catalog). Embedded users call
the embedded `Store`'s typed `idx_*` API directly (§5).

Two-layered surface: **typed shortcuts** for common forms plus **`*_raw`
argv passthroughs** that keep every server capability reachable without the
client chasing the verb grammar.

| Method | Params | Returns |
|---|---|---|
| `idx_create_range` | `name, prefix, field, ty: IdxType` | `()` |
| `idx_create_raw` | `args: list<bytes>` (everything after `IDX.CREATE`) | `()` |
| `idx_drop` | `name` | `bool` (existed?) |
| `idx_list` | — | `list<IdxInfo>` |
| `idx_query_range` | `name, min, max, limit: uint, cursor: opt<bytes>` | `IdxPage` |
| `idx_query_eq` | `name, value, limit: uint` | `IdxPage` |
| `idx_query_match` | `name, text, limit: uint` | `list<(key: bytes, score: f64)>` (BM25, best first) |
| `idx_query_knn` | `name, vector: list<f32>, k: uint` | `list<(key: bytes, distance: f64)>` (nearest first) |
| `idx_query_raw` | `args: list<bytes>` | raw `Reply` |

`idx_create_range` builds `IDX.CREATE name ON PREFIX prefix FIELD field TYPE
<ty> KIND range`. `idx_query_knn` encodes `vector` as an **f32
little-endian blob**. `idx_query_range`/`_eq` return one page in
`(value, key)` order; resume with `IdxPage.cursor`.

**Full IDX.* server surface** (reachable via the `*_raw` passthroughs, and
what the ports SHOULD offer typed helpers for over time — from
`docs/verb-reference.md`):

- `IDX.CREATE name ON PREFIX prefix FIELD field TYPE i64|f64|str|vector KIND
  range|unique|text|ann|agg [MAXMEM bytes] [DIM dim] [DISTANCE cosine|l2|ip]
  [M m] [EF ef] [GROUPBY field]`
- `IDX.DROP name`
- `IDX.LIST`
- `IDX.QUERY name RANGE min max [LIMIT n] [CURSOR c] [FIELDS field …]`
  `| name EQ value [LIMIT n] [CURSOR c] [FIELDS …]`
  `| name MATCH text [LIMIT n] [FIELDS …]`
  `| name KNN vector [LIMIT k] [EF ef] [FIELDS …]`
  `| name GROUP group | name GROUPS [BY count|sum|min|max] [LIMIT n]`
  `| HYBRID text_idx MATCH text ann_idx KNN vector [LIMIT n] [RRFK k] [EF ef] [FIELDS …]`
  `| COMPOSE AND|OR nameA RANGE/EQ … nameB RANGE/EQ … [LIMIT n] [CURSOR c] [FIELDS …]`
- `IDX.COUNT name RANGE min max | EQ value`
- `IDX.EXPLAIN name RANGE min max|EQ value|MATCH text|KNN vector|GROUPS [args …]`
- `IDX.REBUILD name` (ANN tombstone compaction)
- `IDX.VERIFY name` (re-read every held entry; reports
  entries/bytes/coerce_failures/duplicates/drift/checked)

**Vector search (ANN)** and **full-text search (BM25)** are NOT separate verb
families in kevy — they are folded into `IDX.*`:
- vector = `IDX.CREATE … TYPE vector KIND ann DIM d [DISTANCE …] [M …] [EF …]`
  then `IDX.QUERY … KNN <f32-le-blob> LIMIT k [EF ef]`.
- full-text = `IDX.CREATE … TYPE str KIND text` then
  `IDX.QUERY … MATCH "<text>" LIMIT n`.
The typed `idx_query_knn` / `idx_query_match` shortcuts cover the common
query path; `idx_create_raw` covers the create path with its ANN/text options.

### 3.9 Views (`VIEW.*`) — **Remote-only, raw passthrough**

Views are NOT wrapped in the Rust `kevy-client` today. Ports MUST expose them
through the same raw-argv channel used for `idx_query_raw`/pipeline (or add
typed helpers). The server surface (from `docs/verb-reference.md`):

- `VIEW.CREATE name QUERY tree ORDER BY index [DESC] [MODE virtual|materialized]
  [TOPK k] [VIA template]` where `tree = '( AND|OR|DIFF sub sub )' |
  'index RANGE min max' | 'index EQ value'`
- `VIEW.DROP name`
- `VIEW.LIST`
- `VIEW.QUERY name [LIMIT n] [CURSOR c] [FIELDS field …]`
- `VIEW.EXPLAIN name`
- `VIEW.REBUILD name` (materialized only)
- `VIEW.VERIFY name`

A view is a named composition tree over declared indexes storing membership +
order only (never field values).

### 3.10 Change feed / CDC (`FEED.*`)

Both backends serve the same cursor contract. Embedded is single-shard
(shard `0`) and requires the store opened with feed enabled (§5); otherwise
`Unsupported`. **Caveat for C-ABI ports:** the C ABI (§5.1) `kevy_open` /
`kevy_open_mem` take no feed-enable flag, so a pure-FFI embedded store cannot
turn the feed on — C-ABI ports MUST return `Unsupported` for embedded
`feed_*` (the Rust in-process client, which links `kevy-embedded` and calls
`with_feed`, is the only embedded path that serves CDC today). Remote `FEED.*`
requires the server started with feed enabled. Adding a feed flag to
`kevy_open` is a tracked future ABI extension; until then, embedded CDC =
remote or Rust-embedded only.

| Method | Params | Returns | Notes |
|---|---|---|---|
| `feed_shards` | — | `count` | embedded: always 1 |
| `feed_tail` | `shard: uint` | `(generation: uint, next_offset: uint)` | where a fresh/resumed consumer begins |
| `feed_read` | `shard: uint, generation: uint, offset: uint, count: opt<uint>, prefixes: list<bytes>` | `FeedBatch` | up to `count` frames (server default 256, hard cap 4096) past the cursor, optional key-prefix filter |

**Resync contract:** an unservable cursor (stale generation / evicted
offsets) surfaces as a `Protocol` error whose message **starts with
`FEEDRESYNC <gen> <tail>`**. On seeing it: rebuild from a scan, then resume
from `(<gen>, <tail>)`. A cursor ahead of the stream surfaces as
`Protocol("ERR feed cursor ahead of stream")`. Embedded feed with a non-zero
shard → `InvalidInput`. The prefix filter is **fail-open**: multi-key/keyless
verbs (`DEL`, `MSET`, `RENAME`, `*STORE`, …) are never dropped.

Resume from `(batch.generation, batch.next_offset)` on the next `feed_read`.

Server surface: `FEED.READ shard generation offset [COUNT n] [PREFIX p …]`,
`FEED.SHARDS`, `FEED.TAIL shard`.

### 3.11 Pub/sub — consumer side (`Subscriber`)

A subscribed connection cannot send normal commands, so the **consumer side
is a distinct type** (`Subscriber`), separate from the command `Connection`.
Publishing is done from a normal `Connection` via `publish` (§3.1).

Backends by URL: `kevy://`/`redis://`/`tcp://` → dedicated TCP socket;
`mem://<name>`/`file:///path` → in-process bus. **Anonymous `mem://` is
rejected** (`Unsupported` — no other producer; use a named bus).

| Method | Params | Returns | Notes |
|---|---|---|---|
| `Subscriber.connect` | `url` | `Subscriber` | open, subscribe nothing yet |
| `Subscriber.connect_channels` | `url, channels: list` (≥1) | `Subscriber` | connect + subscribe; empty → `InvalidInput`; **subscribed when it returns** |
| `subscribe` | `channels: list` (≥1) | `()` | **returns once every channel is acked** — see below |
| `psubscribe` | `patterns: list` (≥1) | `()` | Redis glob (`*`,`?`,`[…]`); same ack rule as `subscribe` |
| `unsubscribe` | `channels: list` (empty = all) | `()` | |
| `punsubscribe` | `patterns: list` (empty = all) | `()` | |
| `recv` | — | `PubsubEvent` | block for next frame (acks + deliveries); close → `Closed` |
| `recv_message` | — | `(channel: bytes, payload: bytes)` | skip ack frames, return next `Message`/`Pmessage` (pattern discarded) |

**`subscribe` / `psubscribe` are subscribed-on-return.** They send the
command and then read until the server has acked every channel or pattern.
A caller may publish — or cause a publish — immediately after they return
and be certain the subscription is live.

The acks are **queued, not consumed**: they still come out of `recv` in
arrival order, so the observable event stream is unchanged. Anything else
that arrives while waiting (a message on an already-subscribed channel is
normal, and can legitimately precede the ack for a new one) is queued
ahead of them rather than dropped — dropping it would trade a race for a
lost message.

Embedded backends register synchronously in-process and have no such
window; the rule is about remote (RESP over TCP) subscribers.

Rationale: before this rule, `subscribe` wrote the command and returned,
so it handed back a subscriber that was not yet subscribed. Anyone who
published straight after was racing the registration, and a lost message
parks a blocking `recv_message` forever. Three independent tests raced
exactly that way in one day — including a CI job that hung for 3h46m. See
`bench/FINDING-2026-07-19-subscribe-returns-before-live.md`.

| Method | Params | Returns | Notes |
|---|---|---|---|
| `hello3` | — | `PubsubEvent` (synthetic Subscribe marker) | negotiate RESP3 push frames; **remote-only** (embedded → `Unsupported`); must precede any subscribe |
| `set_read_timeout` | `dur: opt<duration>` | `()` | bounded blocking; timeout surfaces as `Io`(WouldBlock/TimedOut) |
| `events` (iterator/stream) | — | stream of `PubsubEvent` | terminates on `Closed`; other errors yielded |
| `messages` (iterator/stream) | — | stream of `(channel, payload)` | ack frames silently skipped |

`recv` accepts both RESP2 array (`*N`) and RESP3 push (`>N`) delivery shapes
transparently.

### 3.12 Transactions (`MULTI`/`EXEC`/`DISCARD` + `WATCH`) — **Remote-only**

Embedded rejects `multi`/`watch`/`unwatch` with `Unsupported` (single-
connection embedded access is already serial). Wire flow: `MULTI` → `+OK`;
each queued command → `+QUEUED`; `EXEC` → array of N typed replies (one per
queued command). A `WATCH` violation makes `EXEC` return Nil (transaction
aborted).

Connection-level:

| Method | Params | Returns |
|---|---|---|
| `watch` | `keys: list` (≥1) | `()` |
| `unwatch` | — | `()` |
| `multi` | — | `Transaction` handle |

`Transaction` handle:

| Method | Params | Returns | Notes |
|---|---|---|---|
| `queue` | `parts: list<bytes>` (≥ verb) | `()` | raw argv passthrough; expects `+QUEUED` |
| typed builders | `set/get/del/exists/incr/incr_by/mget/mset` (same arg shapes as §3.1) | chainable self | each expects `+QUEUED` |
| `exec` | — | `list<Reply>` | Nil (WATCH abort) collapses to empty list (legacy) |
| `exec_watched` | — | `opt<list<Reply>>` | `None` on WATCH abort, `Some([])` on empty success |
| `exec_typed` | — | `TransactionReplies` cursor | WATCH abort → `Protocol` error |
| `exec_watched_typed` | — | `opt<TransactionReplies>` | `None` on abort |
| `discard` | — | `()` | abandon queued commands |

**Drop/close semantics:** a `Transaction` abandoned without exec/discard MUST
send an implicit `DISCARD` (so the socket isn't left in MULTI mode). Ports
without deterministic destructors MUST expose an explicit
`discard`/`close`/context-manager and document that dropping without it is a
bug.

### 3.13 Pipeline (non-atomic batching) — **Remote-only**

Queue N commands client-side, send as one write, read N replies in order.
NOT atomic (server may interleave other clients). Embedded → `Unsupported`.

| Method | Params | Returns | Notes |
|---|---|---|---|
| `pipeline(build)` | a builder callback that appends commands | `list<Reply>` | replies in queue order |
| builder `cmd` | `parts: list<bytes>` | chainable self | empty argv poisons the batch → `InvalidInput` at send |
| builder `len`/`is_empty` | — | `uint`/`bool` | |

Per-command errors come back as `Reply::Error` entries **inline** (they do NOT
abort the batch) — unlike the typed single-command wraps which map `-ERR` to
an error. An empty batch returns `[]` without touching the wire.

The async pipeline (Rust) is a fluent builder:
`pipeline().set(k,v).get(k).incr(c).run(&mut conn) → list<Reply>`, with an
`into_cmds()` escape hatch that hands back raw argv vectors (degrade path to a
blocking client). Ports SHOULD offer the fluent form on the async face.

### 3.14 Blocking pops

Remote lets the server park the connection (a real block). **Embedded via the
C ABI cannot** — the C ABI (§5.1) exposes no blocking-pop symbol, and the
embedded `cmd` dispatcher answers `-ERR unknown command 'BLPOP'` (blocking
verbs live in the server's connection reactor, not argv dispatch). So a
pure-FFI embedded port MUST **emulate** `blpop`/`brpop`/`bzpopmin` by polling
the non-blocking pop on a short interval, waking on a concurrent push
(observably correct; the only difference is a bounded poll latency, not
semantics). The Rust in-process client — which links `kevy-embedded`
directly, not the C ABI — parks on the store condvar; C-ABI ports poll. Both
honor the same timeout contract below.

| Method | Params | Returns | Notes |
|---|---|---|---|
| `blpop` | `keys: list (≥1), timeout: opt<duration>` | `opt<(key, value)>` | head; `None` = timed out |
| `brpop` | `keys: list, timeout` | `opt<(key, value)>` | tail |
| `bzpopmin` | `keys: list, timeout` | `opt<ZPopHit>` = `opt<(key, member, score: f64)>` | lowest-scored member; no `BZPOPMAX` server-side |

`timeout = None` waits forever (wire `0`). **`Some(zero)` → `InvalidInput`**
(ambiguous: wire `0` means forever, so a zero duration cannot mean "poll
once"). Empty `keys` → `InvalidInput`. Timeout is sent as fractional seconds.

### 3.15 Cluster client (`ClusterClient`) — **Remote-only**

One connection per shard, CRC16-slot routed so single-key commands hit their
owner shard directly (no server-side `-MOVED` / forwarding hop). Requires the
server in cluster mode. Topology discovered once at connect via
`CLUSTER SLOTS` (16384 slots).

| Method | Params | Returns | Notes |
|---|---|---|---|
| `ClusterClient.connect` | `host, port` (seed) | `ClusterClient` | opens one conn per distinct shard node |
| `shard_count` | — | `uint` | |
| `request_keyed` | `key, argv` | `Reply` | route to key's owner shard |
| `request_unkeyed` | `argv` | `Reply` | keyless (answered by any shard, uses shard 0) |
| `ping` | — | `()` | any shard (accepts `+PONG` or `+OK`) |
| `publish` | `channel, message` | `count` | process-global pub/sub → any shard |
| `set`/`set_with_ttl`/`get`/`incr`/`incr_by`/`expire`/`persist`/`ttl_ms` | as §3.1 | as §3.1 | key-routed |
| `del`/`exists` | `keys: list` | `count` | routed **per key** and summed (cross-shard OK) |
| `dbsize`/`flushall` | — | `count`/`()` | keyless; server fans out internally (whole-cluster) |
| hash/list/set/zset ops | as §3.2–3.5 | as §3.2–3.5 | single-key → key-routed |
| `sinter`/`sunion`/`sdiff` | `keys: list` | `list<bytes>` | routed by **first** key; all keys must share a slot (use a `{hashtag}`) else server `-MOVED` |

CRC16 hashing MUST match Redis Cluster's `key_hash_slot` (including
`{hashtag}` extraction) so routing agrees with the server.

---

## 4. Public data types (field-by-field)

### 4.1 `Reply` (RESP2 + RESP3 value)

The universal decoded reply. Ports need every variant (RESP3 push/pubsub and
the typed extension replies use them):

```
Reply =
  | Simple(bytes)             // +OK etc.
  | Error(bytes)              // -ERR … (text without leading '-')
  | Int(int)                  // :N
  | Bulk(bytes)               // $len …
  | Nil                       // $-1 (RESP2 null bulk / *-1 null array)
  | Array(list<Reply>)        // *N
  | Map(list<(Reply,Reply)>)  // %N  (RESP3)
  | Set(list<Reply>)          // ~N  (RESP3)
  | Double(f64)               // ,   (RESP3)
  | Boolean(bool)             // #   (RESP3)
  | Verbatim { format: [u8;3], text: bytes } // = (RESP3)
  | BigNumber(bytes)          // (   (RESP3)
  | Null                      // _   (RESP3 null)
  | Push(list<Reply>)         // >N  (RESP3 out-of-band, pub/sub delivery)
  | BlobError(bytes)          // !   (RESP3)
```

RESP version is per-connection (`RespVersion = V2 (default) | V3`),
negotiated by `HELLO 3`. RESP2 is the default; RESP3 adds the 9 extra
prefixes plus push frames.

### 4.2 `IdxType`

`I64 | F64 | Str` → wire tags `i64` / `f64` / `str`. (Vector/ANN indexes use
extra required options; declare those via `idx_create_raw`.)

### 4.3 `IdxRow`

```
IdxRow { key: bytes, value: bytes }   // value = indexed field's wire string form
```

### 4.4 `IdxPage`

```
IdxPage {
  cursor: opt<bytes>,   // None when scan complete (wire cursor "0"); else pass back to idx_query_range
  rows: list<IdxRow>,   // hits in (value, key) order
}
```

### 4.5 `IdxInfo` (one `IDX.LIST` entry)

```
IdxInfo {
  name: bytes,
  prefix: bytes,
  kind: str,      // "range" | "unique" | "text" | "ann" | "agg"
  state: str,     // "ready" | "building"
  entries: uint,  // total indexed entries across shards
  bytes: uint,    // total index bytes across shards
}
```
Parsed from a flat label/value bulk array; **unknown labels are skipped**
(forward-compatible).

### 4.6 `FeedFrame`

```
FeedFrame {
  offset: uint,          // monotonic within a generation
  argv: list<bytes>,     // the applied effect's argv, e.g. ["SET","k","v"]
}
```

### 4.7 `FeedBatch`

```
FeedBatch {
  generation: uint,      // stream's current generation
  next_offset: uint,     // resume cursor
  frames: list<FeedFrame>, // offset order; may be empty (caught up)
}
```

### 4.8 `PubsubEvent` (non-exhaustive — ports must tolerate future kinds)

```
PubsubEvent =
  | Subscribe    { channel: bytes,          count: int }
  | Psubscribe   { pattern: bytes,          count: int }
  | Unsubscribe  { channel: opt<bytes>,     count: int }   // None = "all"/nil bulk
  | Punsubscribe { pattern: opt<bytes>,     count: int }   // None = "all"
  | Message      { channel: bytes,          payload: bytes }
  | Pmessage     { pattern: bytes, channel: bytes, payload: bytes }
```
`count` = total channels + patterns held after the op.

### 4.9 `ZPopHit`

`(key: bytes, member: bytes, score: f64)` — result of `bzpopmin`.

### 4.10 `Transaction` / `TransactionReplies`

`Transaction` is the in-flight MULTI handle (§3.12). `TransactionReplies` is
a typed cursor over a successful EXEC's per-command replies:

| Method | Returns |
|---|---|
| `remaining` | `uint` |
| `expect_empty` | `()` (errors if replies remain — arity gate) |
| `raw` | `Reply` (escape hatch) |
| `next_ok` | `()` (expects `+OK`) |
| `next_ok_or_nil` | `bool` (`+OK`→true, Nil→false; for `SET … NX/XX`) |
| `next_int` | `int` |
| `next_bulk` | `opt<bytes>` (Bulk / Nil) |
| `next_array_of_bulks` | `list<opt<bytes>>` (for MGET) |
| `next_simple` | `bytes` (any simple string, e.g. `+PONG`) |

Each `next_*` consumes one reply; a variant mismatch is a `Protocol` error
(cursor still advances so `expect_empty` stays meaningful).

### 4.11 `PipelineBuf` (sync) / `Pipeline` (async builder)

`PipelineBuf`: `cmd(parts) -> self`, `len()`, `is_empty()`, plus an internal
poison flag for empty-argv. `Pipeline` (async): fluent typed queue methods
(`get`/`set`/`set_with_ttl`/`del`/`exists`/`incr`/`incr_by`/`expire`/
`publish`/`hget`/`hset`/`lpush`/`rpush`/`sadd`), `push_raw(argv)`,
`run(conn) -> list<Reply>`, `into_cmds() -> list<argv>`, `len`/`is_empty`.

### 4.12 `ClusterClient`

Opaque: holds one connection per distinct shard node (advertised order) plus
a `slot → shard-index` table of length 16384. Methods in §3.15.

### 4.13 Enums shared with the embedded store

`ZAggregate = Sum | Min | Max`; `HExpireCond = Always | Nx | Xx | Gt | Lt`;
`HExpireCode = i8`; `EvictionPolicy` (§5); `StoreError` (§2.3).

---

## 5. kevy-embedded contract (in-process `Store`)

Each language also ships a **kevy-embedded** — the in-process store the
`mem://` / `file://` client URLs use. The canonical minimal shape is the
already-shipped **C ABI** in `crates/kevy-ffi` (every non-Rust door binds to
it). The ABI is deliberately tiny: **there is no per-verb C function** — one
`kevy_cmd` takes argv and returns the RESP-encoded reply, so all ~184 verbs
are reachable through one symbol and a new verb needs zero ABI change. Each
binding pairs it with a small RESP parser.

### 5.1 C ABI surface (canonical; `KEVY_ABI = 1`)

Handles: `KevyDb` (opaque store), `KevySub` (opaque subscription).
Buffer: `KevyBuf { ptr: *u8, len: usize, cap: usize }` — kevy-owned, freed via
`kevy_buf_free` (passed as three scalars, not the struct by value, for
AArch64/bun:ffi compatibility). `ptr` is null when `len == 0`.

| Symbol | Signature (C) | Semantics |
|---|---|---|
| `kevy_abi` | `u32()` | ABI version |
| `kevy_version` | `*const char()` | engine version, static NUL-terminated |
| `kevy_open` | `*KevyDb(dir: *u8, dir_len)` | **open persistent** store rooted at `dir` (UTF-8, not NUL-terminated); null on failure |
| `kevy_open_mem` | `*KevyDb()` | **open in-memory** store (nothing survives the process) |
| `kevy_close` | `void(*KevyDb)` | close + free; null is no-op; pass exactly once |
| `kevy_cmd` | `i32(db, argc, argv: **u8, argv_len: *usize, out: *KevyBuf)` | **execute one command**; RESP reply → `out`. Returns `0` on success. A protocol `-ERR` is still a **successful** call (RESP error in `out`). Non-zero = misuse (null/zero-argc/panic), `out` empty |
| `kevy_buf_free` | `void(ptr, len, cap)` | free a returned buffer; null ptr is no-op; free exactly once |
| `kevy_get` | `i32(db, key: *u8, key_len, out: *KevyBuf)` | **scalar fast GET** (no argv/RESP): `1` hit (bytes in `out`), `0` miss, negative misuse |
| `kevy_set` | `i32(db, key, key_len, val, val_len, ttl_ms: u64)` | **scalar fast SET** (`ttl_ms == 0` = no TTL): `0` ok, negative misuse/storage error |
| `kevy_subscribe` | `*KevySub(db, chan: *u8, chan_len)` | open a channel subscription; null on error |
| `kevy_psubscribe` | `*KevySub(db, pat, pat_len)` | open a glob-pattern subscription |
| `kevy_sub_next` | `i32(sub, out: *KevyBuf)` | **poll** one frame non-blocking: `1` frame (RESP array in `out`), `0` empty, negative misuse |
| `kevy_sub_wait` | `i32(sub, timeout_ms: u64, out)` | **block** up to `timeout_ms` (`0` = forever): `1` frame, `0` timeout, negative misuse/bus-gone |
| `kevy_sub_close` | `void(sub)` | close subscription; null no-op; once |

Pub/sub is **polled, not callback** (a callback across FFI on the publisher's
thread is a reentrancy / GC-interop hazard). Each frame is encoded as the
**same RESP array the server would push**: `*3 message <channel> <payload>`,
`*4 pmessage <pattern> <channel> <payload>`, and `*3` subscribe/unsubscribe
acks (`<kind> <name|nil> :<count>`). Every entry point catches panics
(unwinding across `extern "C"` is UB; this is a trust boundary).

Binding helper (Rust-side, not C ABI): `unpack_argv(packed)` decodes the
packed argv the byte-array bindings (JNI, N-API) send — each argument a
`u32-LE` length prefix + that many bytes, back to back; `None` on truncation
or zero args.

### 5.2 The port's embedded surface (minimum)

Every language's kevy-embedded MUST expose, on its own idiomatic object:

- `open(dir)` / `openMem()` — persistent vs in-memory.
- `close()` — release (idempotent-safe, once).
- `cmd(argv) -> Reply` (or RESP bytes the caller parses) — the universal
  command path; this is how the embedded client backend runs *every* verb
  (string/hash/list/set/zset/IDX.*/VIEW.*/FEED.* alike).
- `get(key)` / `set(key, value, ttl_ms?)` — the scalar fast paths.
- `subscribe(channel)` / `psubscribe(pattern)` → a subscription handle with
  `next()` (poll) and `wait(timeout)` (block), yielding RESP-encoded frames;
  `close()`.

### 5.3 Persistence + replay (config on open)

The embedded `Store` open path carries configuration (Rust `Config` builder;
ports expose the equivalent options on `open`):

- **Persistence:** `with_persist(dir)` — AOF auto-append on every write +
  replay on open (snapshot `dump-0.rdb` loaded first, then AOF `aof-0.aof`
  replayed on top); restart-safe. Flushes AOF on close/drop. Tuning:
  `with_appendfsync`, `with_auto_aof_rewrite(pct, min_size)`,
  `with_max_memory(bytes)` + `with_eviction(EvictionPolicy)`,
  `with_shards(n)`, `with_reaper_interval`, `with_ttl_reaper_manual`.
- **Change feed (CDC):** `with_feed(buffer_size)` — required for the
  `feed_*` API on the embedded backend; single shard (`0`).
- **Replication:** `open_replica(upstream)` / `with_replica_upstream` /
  `with_replica_id` / `with_replica_reconnect(min, max)` (embed-as-replica),
  and `with_embed_writer(bind_addr)` / `with_embed_writer_backlog`
  (embed-as-writer). `is_replica()`, `set_replica_upstream(...)`. A write to
  a read-only replica surfaces `KevyError::ReadOnly`.
- **Listener:** `with_resp_listener(addr)` — an in-process read-only RESP
  listener.

`EvictionPolicy = NoEviction (default) | AllKeysLru | AllKeysLfu |
AllKeysRandom | VolatileLru | VolatileLfu | VolatileRandom | VolatileTtl`.

The richer Rust typed embedded API (`Store::get/set/del/hset/…/idx_*/…`,
`Store::with(|inner| …)`, `collect_keys`, `tick`, `subscribe`, `publish`,
`changes_since`) is what the sync embedded client backend delegates to; ports
MAY expose typed embedded methods too, but the **conformance minimum** is the
`cmd(argv)` + scalar + subscribe surface of §5.2.

**wasm targets** have no `Instant`/`SystemTime`: the port must feed clocks
(`set_clock_ns` monotonic ns; `set_wall_clock_ms` epoch ms) before
TTL-sensitive ops (kevy-embedded exposes both on `wasm32-unknown-unknown`).

---

## 6. TDD conformance checklist

Every port's "kevy client contract tests" MUST cover the following. Each
bullet is a language-agnostic behavior; a port passes when its test suite
asserts it on **both** the embedded and remote backend (except where marked
remote-only).

**Connection & URL routing**
- [ ] `mem://` opens an isolated embedded store; two `mem://` are independent.
- [ ] `mem://<name>` (or `file://path`) opened twice shares one store + bus.
- [ ] `kevy://`/`redis://`/`tcp://` open TCP; `redis://…/N` and `kevy://…/N`
      do a `SELECT N`; `tcp://` does not.
- [ ] `rediss://`/`kevys://` → Unsupported; `redis://u:p@h` → Unsupported;
      unknown scheme → InvalidInput; `file://` empty path → InvalidInput.
- [ ] Sync and async faces exist on ONE client and agree on results.

**Error-as-value / exception mapping**
- [ ] A `-ERR`/`-WRONGTYPE` reply surfaces as a Protocol/Store error with the
      wire text preserved (not a transport error).
- [ ] A wrong-type op surfaces `Store(WrongType)`; `INCR` on non-numeric →
      `Store(NotInteger)`.
- [ ] Embedded IDX.*/MULTI/pipeline → Unsupported.
- [ ] Server close mid-read → Closed; bounded blocking timeout → the timeout
      error/`None`.

**Core KV**
- [ ] set/get/del/exists/incr/incr_by round-trips; `get` missing → null.
- [ ] expire/persist/ttl_ms (`-2` no key, `-1` no TTL); set_with_ttl atomic.
- [ ] type_of returns the right type name / "none"; dbsize; flushall wipes.
- [ ] mget order + nulls; mset atomic.

**Collections**
- [ ] hash: hset newly-added count, hget/hdel/hlen/hgetall(flat)/hkeys/hvals.
- [ ] list: lpush/rpush lengths, lpop/rpop counts, llen, lrange negatives.
- [ ] set: sadd/srem counts, smembers, scard, sismember, sinter/sunion/sdiff.
- [ ] zset: zadd, zscore, zcard, zrange asc, zrem.

**Sorted-set algebra**
- [ ] zinterstore/zunionstore cardinality; `_with` WEIGHTS + AGGREGATE
      SUM/MIN/MAX; zintercard with/without LIMIT; empty keys → InvalidInput.

**Hash-field TTL**
- [ ] hexpire (whole-second) / hpexpire (ms) return per-field codes in order
      (`1` set, `-2` missing); httl (secs) / hpttl (ms); hpersist codes;
      empty fields → InvalidInput.

**Pub/sub round-trip**
- [ ] publish → subscriber `recv`/`recv_message` receives `Message`; pattern
      subscribe receives `Pmessage`; ack frames delivered via `recv`, skipped
      by `recv_message`.
- [ ] Anonymous `mem://` Subscriber → Unsupported; named/file bus works
      cross-`connect`.
- [ ] (remote) `hello3` upgrades to RESP3 push frames; `recv` handles both
      RESP2 arrays and RESP3 push transparently.
- [ ] read-timeout / cancellation bounds a blocking `recv`.

**Transactions (remote-only)**
- [ ] MULTI → queue → EXEC returns N replies in order; typed builders +
      `exec_typed` cursor extractors; `expect_empty` arity gate.
- [ ] WATCH + concurrent modify → `exec_watched` returns null (abort);
      `exec_typed` on abort → error.
- [ ] abandon-without-exec sends implicit DISCARD (socket not stuck in MULTI).

**Pipeline (remote-only)**
- [ ] N commands one round-trip, replies in order; per-command `-ERR` lands
      inline as `Reply::Error` (batch not aborted); empty batch → no wire I/O;
      empty-argv → InvalidInput. Async fluent builder + `into_cmds` degrade.

**Blocking pops**
- [ ] blpop/brpop immediate hit returns `(key,value)`; empty-with-timeout →
      null; bzpopmin lowest score `(key,member,score)`; `Some(0)` timeout and
      empty keys → InvalidInput.

**IDX query (remote-only)**
- [ ] idx_create_range then idx_query_range paging (cursor advances, ends at
      null); idx_query_eq; idx_query_match `(key,score)` BM25 order;
      idx_query_knn `(key,distance)` with f32-LE blob; idx_list parses
      IdxInfo incl. unknown-label skip; raw passthroughs reach COMPOSE/HYBRID.

**FEED replay**
- [ ] feed_shards; feed_tail cursor; feed_read returns FeedBatch with frames +
      next cursor; resume from `(generation,next_offset)`; a stale cursor →
      `FEEDRESYNC <gen> <tail>` error → rebuild path; prefix filter fail-open;
      embedded requires feed-enabled config (else Unsupported) and shard 0.

**Cluster (remote-only)**
- [ ] `CLUSTER SLOTS` topology parse + CRC16 routing agrees with the server
      (`-MOVED` never fires for correct keys); del/exists route-per-key + sum;
      dbsize/flushall whole-cluster; set-combine same-slot requirement.

**Embedded store contract**
- [ ] open/openMem/close; `cmd(argv)` runs an arbitrary verb and returns a
      parseable RESP reply; scalar get/set (with ttl) fast paths; subscribe →
      poll `next()` + blocking `wait(timeout)` yield RESP frames.
- [ ] persistence: write, close, reopen same dir → state survives (snapshot +
      AOF replay).

**Reconnect / robustness**
- [ ] a dropped remote connection surfaces `Closed`/`Io`; the client can
      reconnect on a fresh `connect` (or the port's reconnect policy) and
      resume commands.

---

## 7. Per-language adaptation notes (Rust-idiom-specific flags)

- **`&[u8]` everywhere** — all keys/values are binary-safe byte strings. Ports
  in string-first languages (Python `str`, JS `string`, Java `String`) MUST
  accept bytes and NOT force UTF-8 on user data (offer both `bytes` and
  `str` overloads; the wire is bytes).
- **`Duration`** — TTL params are durations; map to the language's duration
  type or an explicit ms/sec integer, but preserve the whole-second vs
  millisecond distinction between `hexpire`/`httl` and `hpexpire`/`hpttl`.
- **`Option<T>`** — nullable returns (`get`, `zscore`, `randomkey`, cursor)
  map to the language's nullable/optional; do NOT collapse "absent" into an
  empty value.
- **Iterators (`events`/`messages`)** — the Rust reference exposes borrowing
  iterators; async ports SHOULD expose these as async streams/async iterators
  (`AsyncIterator`, channel, generator), and Rust's async subscriber
  deliberately omits `set_read_timeout` in favor of runtime-level timeouts —
  ports wrap `recv` with their own timeout primitive.
- **`Transaction` Drop = implicit DISCARD** — languages without deterministic
  destructors MUST provide an explicit close / context manager / `using`
  block and document the leak-avoidance requirement.
- **Enums** — `IdxType`, `ZAggregate`, `HExpireCond`, `EvictionPolicy`,
  `RespVersion` map to native enums; `HExpireCode` is a signed-8-bit code
  (keep it an `int`, not a bool).
- **f32-LE vector blob** — `idx_query_knn` must serialize the query vector as
  little-endian f32 bytes regardless of host endianness.
- **CRC16 slot** — cluster routing must reproduce Redis's `key_hash_slot`
  (16384 slots + `{hashtag}` extraction) exactly.
- **Sync/async split is Rust-only** — the two Rust crates
  (`kevy-client` + `kevy-client-async`) are a crate-hygiene artefact; ports
  unify into one client per the §1.4 hard requirement.
- **Raw-argv escape hatch is mandatory** — because typed wraps intentionally
  lag the full verb grammar (VIEW.*, IDX COMPOSE/HYBRID/GROUPS, future verbs),
  every port MUST expose a raw command channel (`cmd(argv) -> Reply` embedded;
  `request(argv)`/pipeline `push_raw` remote) so no server capability is
  unreachable.
