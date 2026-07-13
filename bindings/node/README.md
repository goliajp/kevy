# @goliapkg/kevy-node

kevy embedded in **Node and Bun** — the real native engine in your
process, no server. One typed surface, `cmd()` to every verb, TTL,
structures, pub/sub, and persistence you can read (AOF + snapshots).

- **Bun** loads the engine over `bun:ffi` — no addon, no build step.
- **Node** loads a hand-written N-API addon (prebuilt per platform).
- Both come from the platform package `optionalDependencies` resolve;
  nothing compiles on install.

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

Same API shape as [`@goliapkg/kevy`](https://www.npmjs.com/package/@goliapkg/kevy)
(the browser/wasm build) and every other kevy embedding. Docs:
<https://kevy.golia.jp>.
