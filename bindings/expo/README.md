# expo-kevy

kevy embedded in **React Native** via Expo Modules — the real native
engine in your app, synchronous JSI calls (the MMKV shape), plus
everything an mmap KV doesn't have: real TTL, hashes/lists/zsets,
pub/sub, `cmd()` to every verb, and persistence you can read (AOF +
snapshots). Works in Expo apps and — via `expo-modules-core` — in bare
React Native apps.

```bash
npx expo install expo-kevy
```

```ts
import { open } from "expo-kevy";
import * as FileSystem from "expo-file-system";

const db = open({ dir: `${FileSystem.documentDirectory}kevy` });

db.set("session:7f3a", "payload", { ttlMs: 3_600_000 }); // scalar fast path
db.getText("session:7f3a");

db.subscribe("room", (payload, channel) => { /* pump-driven */ });
db.publish("room", "hi");

// The escape hatch: every verb, RESP semantics, errors as VALUES.
const reply = db.cmd("ZADD", "board", "42", "alice");

db.close();
```

`open()` is synchronous, `set`/`get` ride the C ABI's scalar fast path
— that is the MMKV lane. Typed methods throw on a protocol error;
`cmd()` returns `KevyError` as a value. The native shells (Swift over
`Kevy.xcframework`, Kotlin over `libkevy_jni`) are bare pipes: the
whole typed surface lives in TypeScript, same API shape as
`@goliapkg/kevy-node` and `@goliapkg/kevy` (wasm).

Docs: <https://kevy.golia.jp>.
