# @goliajp/kevy

[kevy](https://github.com/goliajp/kevy) in the browser: a Redis-compatible KV engine (values, TTLs, counters, pub/sub) compiled to WebAssembly, with real persistence (OPFS, IndexedDB fallback) and cross-tab pub/sub. Zero dependencies — a bare wasm module plus a hand-written ES-module loader.

```js
import { open } from "@goliajp/kevy";

const db = await open({ persist: { name: "app" } });

db.set("greeting", "hello");
db.set("session", "abc123", { ttlMs: 60_000 });
db.getText("greeting");            // "hello"
db.incrby("visits");               // 1, 2, 3, ...
db.keys("user:*");

// Survives reloads: writes stream to OPFS as a kevy append-only log
// and replay on the next open(). await db.flush() is the durability
// barrier; compaction is automatic.

// Pub/sub — including other tabs of the same origin:
const off = db.subscribe("events", (payload, channel) => {
  console.log(channel, new TextDecoder().decode(payload));
});
db.publish("events", "hi from this or any other tab");
```

- **Storage**: OPFS (`FileSystemSyncAccessHandle` in a worker) is the primary backend; IndexedDB is the automatic fallback. localStorage is deliberately not supported — its ~5 MB quota, synchronous main-thread writes, and string-only storage make it unfit for a write log.
- **Log format**: the persisted log is a standard kevy AOF — bytes written by a browser tab replay in a native kevy server or embedded store unchanged.
- **Cross-tab pub/sub** rides `BroadcastChannel`, at-most-once with no backlog (only currently-open tabs receive a message), matching server pub/sub semantics.
- **TTLs** are driven by a timer the loader installs (default 100 ms); pass `tickMs: 0` and call `tick()` to own the cadence.

The wasm module exports a plain C ABI (no binding generator); see the [`kevy-wasm`](https://docs.rs/kevy-wasm) crate docs to embed it in your own host instead of this loader.

License: Apache-2.0 OR MIT.
