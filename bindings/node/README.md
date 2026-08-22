# @goliapkg/kevy-node

kevy embedded in **Node and Bun** — the real native engine in your
process, no server. One typed surface, `cmd()` to every verb, TTL,
structures, pub/sub, and persistence you can read (AOF + snapshots).

- **Bun** loads the engine over `bun:ffi` — no addon, no build step.
- **Node** loads a hand-written N-API addon (prebuilt per platform).
- Both come from the platform package `optionalDependencies` resolve;
  nothing compiles on install.

> **Pre-release.** This document tracks kevy **5.4.0**. The package is
> not on npm yet, so the command below does not resolve — until it is,
> use the in-repo copy: `npm install /path/to/kevy/bindings/node` after
> `cargo build -p kevy-ffi -p kevy-napi`.

```bash
npm install @goliapkg/kevy-node   # or: bun add @goliapkg/kevy-node
```

```js
import { open, text } from "@goliapkg/kevy-node";

const db = await open({ dir: "data/" }); // or open() for in-memory
db.set("session:7f3a", "payload", { ttlMs: 3_600_000 });
db.getText("session:7f3a");              // "payload"

db.subscribe("room", (payload, channel) => {
  console.log(channel, text(payload));
});
db.publish("room", "hi");

// The escape hatch: every verb, RESP semantics, errors as VALUES.
const reply = db.cmd("ZADD", "board", "42", "alice");

db.close();
```

Typed methods (`set` / `get` / `getText` / `del` / `incrby` / `expire`
/ `pttl` / `keys` / `mget` / `dbsize` / `flushall` / `publish` /
`subscribe`) **throw** on a protocol error — a typed call has one
meaning. `cmd()` returns `KevyError` as a value instead: driving the
raw verb surface, the engine saying no is data.

## Entry points

The top-level `open()` (from `index.js`) is **async** — it picks the
runtime backend with a dynamic `import`. The sub-doors load a backend
directly and expose a **sync** `open()`:

```js
import { open } from "@goliapkg/kevy-node/bun.js";  // Bun, sync
import { open } from "@goliapkg/kevy-node/node.js";  // Node, sync
```

## Prebuilt platforms

Native binaries ship as `optionalDependencies` for:

| Platform | Bun (`bun:ffi`) | Node (N-API) |
| --- | --- | --- |
| darwin-arm64 | ✓ | ✓ |
| linux-x64 | ✓ | ✓ |
| linux-arm64 | ✓ | ✓ |

darwin-x64, win32, and musl (Alpine) are **not** prebuilt: those fall to
the in-repo dev build (`target/debug/…`, resolvable only in a checkout)
or fail to load. Build kevy from source to run there.

Same API shape as [`@goliapkg/kevy`](https://www.npmjs.com/package/@goliapkg/kevy)
(the browser/wasm build) and every other kevy embedding. Docs:
<https://kevy.golia.jp>.
