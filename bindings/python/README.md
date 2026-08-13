# kevy — Python client

First-party Python client for **kevy**, the pure-Rust, Redis-compatible
engine. One package ships **both faces** (contract §1.4): a blocking
`Client` and an asyncio `AsyncClient` — the same shape as redis-py's
`redis` + `redis.asyncio`.

```sh
pip install kevy
```

```python
import kevy

# Remote — native RESP2/3 over TCP. Standard library only.
r = kevy.connect("kevy://127.0.0.1:6379")
r.set("k", "v")
assert r.get(b"k") == b"v"

# Asyncio — same package, same results.
ac = await kevy.AsyncClient.connect("kevy://127.0.0.1:6379")
await ac.set("k", "v")

# Embedded (in-process), over the C ABI with ctypes. Needs the engine —
# see below; this line raises a message naming where to get it otherwise.
c = kevy.connect("mem://app")        # or  file:///var/lib/app
```

## The embedded engine

`mem://` and `file://` run kevy inside your process through
`libkevy_ffi`, a per-platform native library. A Python wheel does not
carry it: it would work on the handful of targets we happened to build
and nowhere else, while the remote client above works everywhere
CPython does. So the engine comes from the engine's own repository —

```sh
git clone https://github.com/goliajp/kevy
cd kevy && cargo build --release -p kevy-ffi   # → target/release/
```

— and is found automatically from a kevy checkout, or by pointing
`KEVY_FFI_LIB` at the built library. Without it, the first embedded call
raises an error saying exactly this. Nothing else in the API differs.

## One URL, two transports (§1.1)

| Scheme | Backend |
|---|---|
| `mem://` | embedded, isolated in-memory store |
| `mem://<name>` | embedded, shared by name (pub/sub works cross-connect) |
| `file:///abs/path` | embedded, persistent (snapshot + AOF) |
| `kevy://` / `redis://` / `tcp://` | remote RESP over TCP |

`rediss://` / `kevys://` (TLS) and `redis://user:pass@host` (AUTH) are
rejected — kevy has neither.

## Embedded door (§5)

`kevy.open_mem()` / `kevy.open_persistent(dir)` return a `DB` — the
in-process store over `libkevy_ffi`, driven by ctypes (no C-extension
build). Every verb is reachable through `db.cmd(*argv) -> Reply`; `db.get`
/ `db.set` are the scalar fast paths; `db.subscribe(chan)` gives a polled
(`next()`) + blocking (`wait(ms)`) subscription. The library is located via
`$KEVY_FFI_LIB` or the repo `target/{release,debug}` build.

## Command families

Every family of the contract (§3): core string/generic, hash, list, set,
sorted set, sorted-set algebra, hash-field TTL, declarative indexes
(`idx_*`, remote-only) — with any other `IDX.*` subcommand reachable through
the raw `do(*argv)` / `idx_query_raw` escape hatches — change feed
(`feed_*`), pub/sub
(`Subscriber` / `AsyncSubscriber`), transactions (`Transaction`),
pipelines, blocking pops (`blpop`/`brpop`/`bzpopmin`), and the cluster
client (`ClusterClient`). Bytes and `str` are both accepted; the wire is
bytes (§7).

## Errors (§2)

Raised as a `KevyError` hierarchy inspectable by type: `StoreError`
(`WrongTypeError`, `NotIntegerError`, …), `ProtocolError`, `IoError`,
`UnsupportedError`, `InvalidInputError`, `NotFoundError`, `ReadOnlyError`,
`TimedOutError`, `ClosedError`.

## Tests

```
pip install -e '.[test]'
pytest                       # embedded + remote (spawns a real kevy server)
```

Remote tests build and boot `kevy` on a temp port; they skip cleanly if the
server binary is absent (`cargo build --release -p kevy`).
