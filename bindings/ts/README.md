# @goliapkg/kevy

The first-party **TypeScript** client for kevy — the pure-Rust
Redis-compatible engine. One `connect(url)` ships both faces of the
[client contract](../../docs/client-contract.md), and the same code runs on
both **Node** and **Bun**:

- **Embedded** (`mem://` / `file://`): the real native engine in your
  process, no server. Loaded over `bun:ffi` on Bun or the N-API addon
  (`@goliapkg/kevy-node-*`) on Node — no system dependency to install.
- **Remote** (`kevy://` / `redis://` / `tcp://`): a native RESP2/RESP3 TCP
  client. Same business code, switch backends by changing only the URL.

```bash
npm install @goliapkg/kevy        # or: bun add @goliapkg/kevy
```

```ts
import { connect, textOf } from "@goliapkg/kevy";

// Embedded in-process, or "kevy://127.0.0.1:6379" for a server — same code.
const c = await connect("mem://app");

await c.set("k", "v");
const v = await c.get("k");                  // Uint8Array | null (binary-safe)
textOf(v!);                                  // "v" — decode bytes to a string
await c.zadd("board", { score: 42, member: "alice" });

// Errors are typed and inspectable by variant.
import { StoreError } from "@goliapkg/kevy";
try {
  await c.incr("k");
} catch (e) {
  if (e instanceof StoreError && e.storeKind === "notInteger") {
    // structured store error
  }
}

// Raw escape hatch: every verb reachable, RESP reply as data.
const reply = await c.do("COMMAND", "COUNT");

await c.close();
```

## Async by default, sync where it's safe

Every command family is a `Promise` method — async is the default because a
remote dial cannot be synchronous. For the embedded backend only, a
synchronous escape hatch lives on `.sync` (contract §1.4 / §7); both faces
bind the **same** engine, so they always agree:

```ts
const c = await connect("mem://app");     // or connectSync("mem://app")
c.sync.set("k", "v");
c.sync.get("k");                          // Uint8Array | null — no await, embedded only
await c.get("k");                         // Uint8Array | null — same value, async face
```

`connectSync(url)` opens an embedded backend without `await`; remote URLs
throw `Unsupported` (dials aren't synchronous). The `.sync` face throws
`Unsupported` on remote-only commands (`IDX.*`, `MULTI`, pipeline).

## Coverage

Core KV, hash, list, set, zset, zset-algebra, hash-field TTL, blocking pops
(`blpop`/`brpop`/`bzpopmin`), `IDX.*` (typed + raw), `VIEW.*`/`FEED.*` (raw
+ typed feed), pub/sub (`Subscriber`), transactions (`MULTI`/`EXEC`/`WATCH`
via `Transaction`), `PipelineBuf`, and a CRC16-routed `ClusterClient`. The
embedded door (`EmbeddedDb`) exposes the raw `cmd` / scalar / `Subscribe`
surface directly.

Erasable TypeScript — no build step. Node strips the types at load
(`--experimental-strip-types`, stable in current Node); Bun runs `.ts`
natively.

## Tests

```bash
npm run test:node        # node --test
npm run test:bun         # bun test
npm run typecheck        # tsc --noEmit
```

Docs: <https://kevy.golia.jp>.
