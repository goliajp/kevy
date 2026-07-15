# react-native-kevy-nitro

kevy for React Native over a **Nitro (JSI)** C++ HybridObject. JS calls a
C++ class *directly via JSI* — no Expo-module dispatch, no packed-argv
marshalling through JNI, no Kotlin hop. The C++ calls `kevy-ffi` (kevy's C
ABI) and passes payloads as `ArrayBuffer` by reference.

Requires the New Architecture (`newArchEnabled=true`) and
`react-native-nitro-modules`.

## API

```ts
import { createKevyNitro } from 'react-native-kevy-nitro'

const kevy = createKevyNitro() // C++ opens an in-memory kevy db

// 1. cmd() — the fast path. Any verb, RESP reply as ArrayBuffer.
const reply = kevy.cmd(packArgv(['SET', 'k', 'v'])) // u32-LE len prefix/arg

// 2. Poll pub/sub — the default. No background thread, no idle CPU.
kevy.subscribe('room')
kevy.publish('room', payload)      // ArrayBuffer
for (let f = kevy.subNext(); f; f = kevy.subNext()) handle(f)

// 3. Batched push — the high-fanout path. A native poller parks on the
//    engine (kevy_sub_wait; ~0% idle CPU) and delivers each burst in one hop.
kevy.subscribePushBatched('room', (frames) => frames.forEach(handle))
// ...later
kevy.stopPush() // joins the poller thread
```

`subscribePush` (one native→JS hop per message) also exists but is a
pessimization on measured workloads — the per-frame CallInvoker hop costs
more than the drain crossings it removes. Prefer `subscribePushBatched`.

## Measured (Android emulator, arm64-v8a, Hermes, new arch)

| | Expo door | Nitro door |
|---|---:|---:|
| `cmd` PING | ~145k/s | ~490k/s (3.4×) |
| pub/sub poll | ~50k/s (Expo pub/sub) | ~500k/s |
| pub/sub batched push | — | ~660k/s (1.3× poll) |

Idle CPU with a live push subscription and no traffic: **~0%** — the poller
parks in the kernel via `kevy_sub_wait` (a spin-poll build burned ~one core).

See `bench/pubsubgate/LEDGER.md` in the kevy repo for the full method and
raw numbers.

## Build

The native engine ships as a prebuilt per-ABI `libkevy_ffi.so` under
`android/src/main/jniLibs/`. Rebuild it from the kevy repo with
`packaging/android/build-ffi-jnilibs.sh`. Regenerate the Nitro bindings with
`npm run specs` (nitrogen). iOS is not wired yet.

## License

Apache-2.0 OR MIT.
