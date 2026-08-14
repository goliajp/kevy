# kevy on WebAssembly

kevy runs in the browser as a real store, not a compile-time curiosity:
the npm package [`@goliapkg/kevy`](https://www.npmjs.com/package/@goliapkg/kevy)
ships the engine (KV + TTL + counters + scans + pub/sub) compiled to
`wasm32-unknown-unknown` behind a hand-written ES-module loader, with
durable persistence over OPFS (IndexedDB fallback) and pub/sub that
crosses tabs. The same crates also build for `wasm32-wasip1`, so the
Rust API works under `wasmtime` / `wasmer` and in edge runtimes.

Try it live: the [kevy.golia.jp demo](https://kevy.golia.jp/demo/) is a
browser REPL over exactly this module — commands, OPFS persistence that
survives reloads, and pub/sub across tabs, with no backend.

## Quick start (browser)

```sh
npm install @goliapkg/kevy
```

```js
import { open } from "@goliapkg/kevy";

const db = await open({ persist: { name: "app" } });

db.set("greeting", "hello");
db.set("session", "abc123", { ttlMs: 60_000 });
db.getText("greeting");            // "hello"
db.incrby("visits");               // 1, 2, 3, ...
db.keys("user:*");

// Pub/sub — including other tabs of the same origin:
const off = db.subscribe("events", (payload, channel) => {
  console.log(channel, new TextDecoder().decode(payload));
});
db.publish("events", "hi from this or any other tab");

await db.flush();                  // durability barrier
```

Writes stream to storage as a kevy append-only log and replay on the
next `open()` with the same `persist.name`. Since 4.0 the log speaks
the checksummed v2 record format (`KEVYAOF2` — see
[persistence.md](persistence.md)): bit-rot in stored bytes is refused
at replay instead of silently applied. A log stored by a pre-4.0 tab
replays unchanged (v1, read forever) and upgrades to v2 at its first
compaction; a log pumped out of a browser tab still replays in a
native kevy unchanged, and vice versa. The package is six files
(496 KB packed, 481 KB gzipped over the wire): the wasm module, the loader, the OPFS worker,
hand-written TypeScript typings, and the usual README + manifest.
Zero dependencies on both sides of the boundary.

## The loader API

`open(options)` instantiates the module, replays the stored log (when
persistence is on), and starts the tick timer and the cross-tab
bridge. Options:

| Option | Default | Meaning |
|---|---|---|
| `wasm` | `kevy.wasm` next to the loader | Module source: URL, `ArrayBuffer`, `Uint8Array`, `Response`, or a compiled `WebAssembly.Module` |
| `persist` | `false` (in-memory) | `{ name, backend }`: one log per `name`; `backend` = `"auto"` (OPFS, IndexedDB fallback), `"opfs"`, or `"idb"` |
| `broadcast` | `true` | Cross-tab pub/sub bridge over `BroadcastChannel` |
| `name` | `persist.name` or `"kevy"` | Instance name; scopes both the storage file and the broadcast channel |
| `tickMs` | `100` | TTL sweep + event poll cadence; `0` disables the timer — call `tick()` yourself |

The returned `Kevy` handle (full signatures in `kevy.d.ts`):

| Method | Semantics |
|---|---|
| `set(key, value, { ttlMs? })` | SET, optionally with an expiry |
| `get(key)` / `getText(key)` | GET as `Uint8Array` / as UTF-8 text; `undefined` when absent or expired |
| `del(key)` / `exists(key)` | DEL / EXISTS |
| `expire(key, ttlMs)` / `persist(key)` / `pttl(key)` | PEXPIRE / PERSIST / PTTL (`-1` no TTL, `-2` no key) |
| `incrby(key, delta?)` | INCRBY, returns the new value |
| `dbsize()` / `flushall()` | DBSIZE / FLUSHALL |
| `keys(pattern?, limit?)` | KEYS with a Redis glob, optionally capped |
| `tick()` | One manual TTL sweep + event poll; returns the expired-key count |
| `subscribe(channel, cb)` / `psubscribe(pattern, cb)` | SUBSCRIBE / PSUBSCRIBE; both return an unsubscribe function |
| `publish(channel, payload)` | PUBLISH to local subscribers and (bridge on) other same-origin tabs; returns the local receiver count |
| `flush()` | Durability barrier: pending write frames are flushed to storage |
| `compact()` | Rewrite storage as a compacted image of the live keyspace |
| `close()` | Flush, tear down bridge/timer/storage, free the instance |

Keys and values are `string | Uint8Array | ArrayBuffer` everywhere:
strings are UTF-8 encoded at the boundary, byte views pass through —
values are genuinely binary, not strings.

## How the binding works (no wasm-bindgen)

kevy's zero-dependency law extends to the toolchain: there is no
binding generator on either side. The [`kevy-wasm`](https://docs.rs/kevy-wasm)
crate exports a flat, hand-written C ABI — 30 `extern "C"` symbols —
and `pkg/kevy.js` is a hand-written loader that owns the TypedArray
boundary, the UTF-8 codecs, the persistence pump, and the cross-tab
bridge. The conventions:

- **Instances** are `u32` handles from `kevy_open`; every other call
  takes the handle first. `0` is never a valid handle.
- **Bytes in** cross as `(ptr, len)` pairs pointing into linear memory
  obtained from `kevy_alloc` / returned with `kevy_free`.
- **Bytes out** land in a per-instance result buffer read via
  `kevy_out_ptr` / `kevy_out_len`, valid until the next call on the
  same handle — callers copy out immediately.
- **Status codes**: `>= 0` is success, `-1` an operation error (UTF-8
  message in the result buffer), `-2` an invalid handle.
- **Numbers** cross as `f64` (clocks, TTLs, counts — all inside the
  2^53 exact-integer range).
- `kevy_abi_version()` reports the ABI contract version, so a loader
  can refuse a mismatched module.

The export surface groups into core (open/close/alloc/free/clock/
tick/result buffer), KV (`kevy_set`, `kevy_get`, `kevy_del`,
`kevy_exists`, `kevy_expire`, `kevy_persist`, `kevy_pttl`,
`kevy_incrby`, `kevy_dbsize`, `kevy_flushall`, `kevy_keys`, …),
pub/sub (`kevy_subscribe`, `kevy_psubscribe`, `kevy_unsubscribe`,
`kevy_publish`, `kevy_poll_events`), and the AOF pump
(`kevy_aof_frames_out`, `kevy_aof_frame_in`, `kevy_aof_dump`). If the
shipped loader doesn't fit your host, the ABI is the supported
integration point — the crate docs specify every symbol and packed
byte format.

## Persistence: a real log, byte-compatible with native kevy

The browser has no filesystem, so durability is host-mediated: with
persistence on, every write also encodes the same RESP multi-bulk
frame a kevy AOF stores on disk (the `kevy-persist` format —
[docs/persistence.md](persistence.md)). The loader pumps pending
frames to storage once per microtask, so a synchronous burst of
writes costs one storage append. `await db.flush()` is the durability
barrier; a resolved `flush()` means the backend has flushed to disk.

**The log a browser tab writes replays in a native kevy unchanged** —
same magic header, same frames. Copy the `.aof` out of OPFS and point
a native embedded store (or the server) at it, and the keyspace comes
back. The reverse holds too: the inbound pump accepts a native-written
log. Corrupt tails follow the native replay contract — the intact
prefix is applied, the tail is dropped, and the next compaction
rewrites storage from live state.

Two backends, picked automatically under `backend: "auto"`:

- **OPFS** (primary): a `FileSystemSyncAccessHandle` owned by a small
  dedicated worker — sync handles are worker-only. Every append
  flushes before acknowledging.
- **IndexedDB** (fallback): the same pump against an object store, for
  contexts without OPFS sync handles (notably older Safari).

Compaction is automatic: once the appended log outgrows
`max(512 KB, 4× the last image)`, the loader rewrites storage as a
compacted image of the live keyspace (the browser-side equivalent of
an AOF rewrite). `compact()` forces one before a snapshot or export.

**localStorage is deliberately not a backend**: a ~5 MB quota, a
synchronous API that blocks the main thread on every write, and
UTF-16 string-only storage disqualify it as a write log.

## Cross-tab pub/sub

`publish()` delivers to subscribers in the same tab through the
engine, and (bridge on) broadcasts the frame over a per-instance
`BroadcastChannel` to every other same-origin tab, where each tab's
local subscribers receive it. Delivery is **at-most-once with no
backlog** — only currently-open tabs receive a message, and nothing
is replayed to a tab that opens later. That is the same contract as
kevy server pub/sub ([docs/pubsub.md](pubsub.md)), so code written
against one face behaves on the other.

## Clocks, ticks, and TTLs

`wasm32-unknown-unknown` has no threads and no OS clock, so the
engine runs with the manual TTL reaper and a host-fed clock: the
loader feeds `Date.now()` through the ABI before each entry point and
runs `tick()` on a timer (default 100 ms). Consequences:

- TTL precision tracks the tick cadence: a key with a 500 ms TTL
  expires on the first tick after its deadline. Set `tickMs` tighter
  if it matters, or `tickMs: 0` to own the cadence entirely.
- No background work happens between your calls — no hidden costs,
  but a forgotten `tick()` (with `tickMs: 0`) leaves expired keys
  live and event queues undrained.

## Performance

Full methodology, environment, and raw numbers:
[bench/WASM-BENCH.md](../bench/WASM-BENCH.md) (headless-Chrome
harness, median of 3 runs, 16-byte values at 1k and 100k keys).
Against the storage a web app actually has:

| Axis (n=100k) | kevy-wasm | vs IndexedDB | vs localStorage |
|---|---:|---:|---:|
| point read (ops/s) | 1.67 M | **77×** | 0.48× |
| point write (ops/s) | 1.79 M | **189×** | 4.9× |
| batch load (ms) | 60.8 | **36× faster** | 4.5× faster |
| scan, ~10% match (ms) | 5.6 | **46× faster** | 5.7× faster |
| durable write (ops/s) | 785 k (OPFS) | **12.6–17.4×** | ~2× |
| restart to usable (ms) | 129 | **2.7× faster** | slower (see below) |

Headlines and the honest caveats:

- **Order of magnitude over IndexedDB on every axis** — 77–86× point
  read, 166–189× point write across dataset sizes.
- **Durable writes 12.6–17.4× raw IndexedDB**: the pump batches write
  frames per microtask, so a write burst costs one storage append.
  It also beats localStorage's fire-and-forget `setItem` ~2× while
  being an actual append log.
- **localStorage point reads are the one column kevy does not win**
  (0.42–0.48×), and the bench documents why no wasm-hosted store can:
  Chrome's localStorage read is a renderer-memory hash lookup (its
  measured 3.5–5 M ops/s is in bare-`Map` class, not storage class),
  while every wasm store pays a UTF-8 encode + boundary crossing +
  copy out of linear memory per call — a ~2–3 M ops/s ceiling that
  kevy sits near (1.7–2.1 M) after the loader's staging
  optimizations (persistent scratch buffer, `encodeInto`, cached
  memory view — a 2.2× gain over the naive loader). What kevy wins is
  everything localStorage cannot do at any speed: binary values,
  TTLs, counters, scans, >5 MB datasets, pub/sub, and non-blocking
  persistence.
- **Cross-tab RTT** beats the storage-event hack at 64 KB payloads
  (p50 0.295 ms vs 0.335 ms) and ties on the ~0.105 ms task-hop floor
  below 1 KB — while the storage-event hack drops frames under a slow
  consumer, is string-only, and writes every ping to disk quota.

## The Rust face: WASI, edge runtimes, direct embedding

The engine itself — `kevy-embedded` and its dependency closure
(`kevy-store`, `kevy-persist`, `kevy-hash`, `kevy-bytes`, `kevy-map`,
`kevy-resp`) — builds for both wasm targets; CI gates the whole list
plus `kevy-wasm` on every push. The network reactor crates
(`kevy-rt`, `kevy-sys`, `kevy-uring`) are deliberately outside the
closure, so the wasm build stays clean.

| Target | Command | Notes |
|---|---|---|
| `wasm32-unknown-unknown` | `cargo build -p kevy-wasm --target wasm32-unknown-unknown --release` | The browser artifact (`kevy_wasm.wasm`); the npm loader instantiates it. For your own host, drive the C ABI directly. |
| `wasm32-unknown-unknown` (Rust API) | `cargo build -p kevy-embedded --target wasm32-unknown-unknown` | No threads, no OS clock: open with `Config::with_ttl_reaper_manual()`, feed `set_clock_ns` / `set_wall_clock_ms`, call `Store::tick()` from the host loop. |
| `wasm32-wasip1` | `cargo build -p kevy-embedded --target wasm32-wasip1` | `Instant` / `SystemTime` work — no clock feeding. `std::fs` works against preopened dirs, so `Config::with_persist("/data")` + `wasmtime --dir=/data` gives real AOF durability. Threads still absent: keep the manual reaper. |

Direct embedding on `wasm32-unknown-unknown` in Rust:

```rust
use kevy_embedded::{Config, Store, set_clock_ns, set_wall_clock_ms};

let store = Store::open(Config::default().with_ttl_reaper_manual())?;

// Host feeds the clock before time-sensitive work…
set_clock_ns(now_ms_from_host().saturating_mul(1_000_000));
set_wall_clock_ms(now_ms_from_host());

store.set(b"hello", b"world")?;
let v = store.get(b"hello")?;   // Some(b"world".to_vec())

// …and drives expiry at its own cadence:
let _stats = store.tick();
```

Errors are `KevyResult` / `KevyError` and argv-style write methods
take borrowed slices, exactly as on native — see the
[`kevy-embedded`](https://docs.rs/kevy-embedded) docs.

Cloudflare Workers and similar edge isolates follow the browser
recipe: one instance per isolate, `Date.now()` as the clock source,
`tick()` lazily or from a scheduled handler. For durability across
isolate restarts, mirror writes to the platform's durable store from
your handler (the AOF pump gives you the frames); inside the isolate
kevy is the hot in-memory tier.

## FAQ

**Does the full command surface work in the browser?** Most of it.
The module is built with every feature a browser can host — `core`,
`persist`, `index`, `text`, `vector` — so `cmd` reaches the KV, TTL,
counter, scan and pub/sub surfaces *and* `IDX.*` (secondary indexes,
full-text, vector search), `VIEW.*` and `TABLE.*`. It was the smaller
cut until 2026-08, which is how the project's own landing page came to
demonstrate secondary indexes against a build compiled without them.

**Which published version has it:** the npm package at 5.1.0 and earlier
carries the smaller cut — `IDX.*`, `VIEW.*` and `TABLE.*` answer
`unknown command` there. The wider build is on the main branch and on
kevy.golia.jp; it reaches npm with the next published version. Build from
the checkout to have it sooner (`cargo build -p kevy-wasm --target
wasm32-unknown-unknown --release`).

What is left out needs something a browser cannot provide rather than
bytes saved: `replicate` a network peer, `listener` a TCP socket,
`tier` a disk directory. Streams, transactions, geo and scripting are
outside the embedded engine's verb surface on every platform — the
boundary is the ESTORE_OPS manifest, not this build.

The Rust API on wasm targets exposes the full `kevy-embedded` feature
set of whichever features you compile in.

**Is the persisted data portable?** Yes — it is a standard kevy AOF.
Browser → native and native → browser both replay. See
[docs/persistence.md](persistence.md) for the format contract.

**What about shared-memory threads (`+atomics`)?** The shipped module
is single-threaded, which matches every browser-style target. The
engine is thread-safe where the host provides threads, but the manual
`tick()` model remains the supported path.

**SharedWorker instead of BroadcastChannel?** A single-owner
SharedWorker topology is cleaner in theory but its availability is
narrower (notably absent on Android Chrome); the BroadcastChannel
bridge won on compatibility with the same measured latency class.

**Why not wasm-bindgen?** kevy ships zero third-party dependencies —
server, embedded, and browser alike. A binding generator would be a
build-time dependency and would own the boundary layout; the
hand-written ABI keeps the contract explicit, versioned, and
auditable.

## In a Tauri app

Running kevy in a desktop/mobile app? A Tauri backend is Rust, so kevy can
embed **natively** in the backend (one shared store, durable, cross-window)
instead of in the webview as wasm. See [docs/tauri.md](tauri.md) for the
backend-store-vs-wasm decision.
