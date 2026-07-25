# kevy — C / C++ client

The first-party **C++** client for kevy — the pure-Rust Redis-compatible
engine — with a **C** façade on top. One `connect(url)` ships both faces of
the [client contract](../../docs/client-contract.md):

- **Embedded** (`mem://` / `file://`): the real native engine in your
  process, no server. Links a static library (`crates/kevy-ffi`) — no
  system dependency to install.
- **Remote** (`kevy://` / `redis://` / `tcp://`): a native RESP2/RESP3 TCP
  client. Same business code, switch backends by changing only the URL.

## C++ (`kevy::Client`)

One umbrella header pulls in the whole surface:

```cpp
#include <kevy/kevy.hpp>

// Embedded in-process, or "kevy://127.0.0.1:6379" for a server — same code.
auto c = kevy::Client::connect("mem://app");

c.set("k", "v");
kevy::OptBytes v = c.get("k");                 // std::optional<std::string>
c.zadd("board", {{42.0, "alice"}});

// Errors are exceptions, one subclass per taxonomy variant.
try {
  c.incr("k");
} catch (const kevy::StoreError& e) {
  // e.store_error() == kevy::StoreErrorKind::NotInteger
}

// Raw escape hatch: every verb reachable, RESP reply as data.
// A -ERR arrives as a Reply with is_error() — data, not a throw.
kevy::Reply r = c.command({"COMMAND", "COUNT"});

c.close();
```

### Both faces, one client

Blocking methods have an async twin via `async()`, which returns an
`AsyncClient` whose methods resolve a `std::future<T>`. They delegate to the
same blocking methods, so sync and async always agree:

```cpp
auto a = c.async();
std::future<kevy::OptBytes> f = a.get("k");
kevy::OptBytes v = f.get();
```

### Coverage

Core KV, hash, list, set, zset, zset-algebra, hash-field TTL, blocking pops,
`IDX.*` (typed + raw), typed `FEED.*` (change feed), pub/sub (`Subscriber`),
transactions (`MULTI`/`EXEC`/`WATCH`), `PipelineBuf`, and a CRC16-routed
cluster client. `VIEW.*` (§3.9) has no typed wrapper — reach it through the
raw `command()` escape hatch. The embedded store is reachable directly through
`kevy/embedded.hpp`. Bytes are never assumed UTF-8; nullable returns use
`std::optional`.

## C (`kevy/c_api.h`)

A C-callable façade over the same client layer — URL routing + both
backends behind one entry point. Functions return an `int` status (`0` ok,
negative = a `KevyError` kind); a protocol `-ERR` is a **successful** call
carrying a RESP error frame in the out-buffer. `kevy_client_last_error()`
returns the last error's text.

(The lower-level *embedded-only* C ABI is the separate
`crates/kevy-ffi/include/kevy.h`; this façade sits on top of the C++ client
so C callers also get URL routing and the remote backend.)

## Build & test

```bash
cargo build -p kevy-ffi                       # the embedded static lib the client links (required)
cargo build --release -p kevy                 # the server the remote tests spawn (see note)
cmake -S bindings/cpp -B bindings/cpp/build
cmake --build bindings/cpp/build
ctest --test-dir bindings/cpp/build           # runs the kevy_tests target
```

The remote-backend tests spawn `target/release/kevy`; without that second build
they **skip** (the embedded tests still run). Point CMake at a different server
binary with `-DKEVY_SERVER_BIN=/path/to/kevy`.

CMake exposes the `kevy_client` static library target; `target_link_libraries(you PUBLIC kevy_client)`.

Docs: <https://kevy.golia.jp>.
