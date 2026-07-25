# tauri-plugin-kevy-api

The guest (webview) binding for [`tauri-plugin-kevy`](../tauri-plugin-kevy): a
typed kevy client over Tauri's `invoke`, backed by one shared in-process kevy
engine in the Rust backend.

```bash
npm install tauri-plugin-kevy-api
```

Requires the Rust side to register the plugin and a capability granting
`kevy:default` (see the [binding README](../README.md)).

## Use

```ts
import { kevy, replyInt, replyString } from 'tauri-plugin-kevy-api'

await kevy.set('greeting', 'hello', { ttlMs: 60_000 })
await kevy.getString('greeting')            // "hello"
await kevy.incr('counter')                  // 1
await kevy.del('greeting')                  // 1

// Typed families ride the raw path:
await kevy.hset('user:1', 'name', 'ada')    // 1
await kevy.lpush('log', 'a', 'b')           // 2
await kevy.zadd('board', 10, 'ada')         // 1

// Any verb via the raw escape hatch → decoded RESP Reply:
const r = await kevy.cmd(['HGET', 'user:1', 'name'])
replyString(r)                              // "ada"

// Live pub/sub over a Tauri Channel:
const sub = await kevy.subscribe({ channels: ['room'] }, (m) => {
  if (m.kind === 'message') console.log(msgPayloadString(m))
})
await kevy.publish('room', 'ping')
await sub.unsubscribe()
```

## Notes

- **Binary-safe.** Keys/values accept `string | Uint8Array | ArrayBuffer |
  number[]`; strings are UTF-8 encoded. Reply byte payloads arrive as
  `number[]` — use `replyBytes` / `replyString` / `replyBytesList` to decode.
- **One client, one store.** Every call reaches the single backend `Store`; use
  the exported `kevy` singleton or construct your own `KevyClient`.
- **`Reply`** mirrors the kevy [client contract](../../../docs/client-contract.md)
  §4.1 (`{ type: 'bulk' | 'int' | 'array' | … }`).
- **Always `unsubscribe()`** on teardown — the subscription's poller lives in
  Rust; dropping the JS handle alone does not stop it.

## Build

```bash
npm install
npm run typecheck   # tsc --noEmit
npm run build       # tsc → dist/
```
