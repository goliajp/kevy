# react-native-kevy-nitro

kevy for React Native over a **Nitro (JSI)** C++ HybridObject. JS calls a
C++ class *directly via JSI* — no Expo-module dispatch, no packed-argv
marshalling through JNI, no Kotlin hop. The C++ calls `kevy-ffi` (kevy's C
ABI) and passes payloads as `ArrayBuffer` by reference.

Requires the New Architecture (`newArchEnabled=true`) and
`react-native-nitro-modules`.

## API

```ts
import {
  createKevyNitro,
  createKevyNitroAt,
  unpackFrames,
} from 'react-native-kevy-nitro'

const kevy = createKevyNitro() // C++ opens an in-memory kevy db

// 1. getData/setData — the scalar KV door (MMKV-shaped). Raw key/value
//    ArrayBuffers straight into kevy_get/kevy_set: no argv packing, no RESP
//    framing, no JS decode. The fastest KV lane.
kevy.setData(keyAB, valueAB, 0)          // ttlMs (0 = no TTL); returns void
const value = kevy.getData(keyAB)        // ArrayBuffer | undefined (miss)

// 2. cmd() — the general fast path. Any verb, RESP reply as ArrayBuffer.
const reply = kevy.cmd(packArgv(['INCR', 'n'])) // u32-LE len prefix/arg

// 3. Poll pub/sub — the default. No background thread, no idle CPU.
kevy.subscribe('room')
kevy.publish('room', payload)            // ArrayBuffer
for (let f = kevy.subNext(); f; f = kevy.subNext()) handle(f)

// 4. Batched push — the high-fanout path. A native poller parks on the
//    engine (kevy_sub_wait; ~0% idle CPU) and delivers each burst in ONE hop
//    as ONE packed ArrayBuffer; unpackFrames slices zero-copy views back out.
kevy.subscribePushBatched('room', (packed, count) => {
  for (const frame of unpackFrames(packed, count)) handle(frame) // Uint8Array
})
// ...later
kevy.stopPush() // joins the poller thread
```

`subscribePush` (one native→JS hop per message) also exists but is a
pessimization on measured workloads — the per-frame CallInvoker hop costs
more than the drain crossings it removes. Prefer `subscribePushBatched`.

### Durable (file-backed) vs in-memory

`createKevyNitro()` opens an **in-memory** db — fastest, but nothing survives
the process. MMKV is a *persistent* store, so a fair KV comparison needs kevy
durable too. `createKevyNitroAt(dir)` opens the db **file-backed** (AOF,
replayed on open) at `dir` — durable across launches, at the cost of the
append-log write per set:

```ts
const kevy = createKevyNitroAt(FileSystem.documentDirectory + 'kevy')
```

Perf/durability tradeoff (see `bench/PERF-DECOMP-2026-07-16-rn-mmkv-mitt.md`
and `bench/PERF-FINDING-2026-07-15-mmap-aof-refuted.md` for the ground
truth): in-memory `setData` (~35 ns FFI CPU, host) crushes MMKV but is
ephemeral. Durable `setData` is gated by the **AOF-vs-mmap** axis, not the
crossing — on real ext4 kevy's durable SET beats MMKV **3.97×** (buffered
append-log amortises to ~3 µs/op vs MMKV's mmap overwrite-in-place). The
scalar door removes the RESP/crossing tax in **both** regimes; durability is
an orthogonal choice.

## Measured (Android emulator, arm64-v8a, Hermes, new arch)

| | Expo door | Nitro door |
|---|---:|---:|
| `cmd` PING | ~145k/s | ~490k/s (3.4×) |
| pub/sub poll | ~50k/s (Expo pub/sub) | ~500k/s |
| pub/sub batched push | — | ~660k/s (1.3× poll) |

Idle CPU with a live push subscription and no traffic: **~0%** — the poller
parks in the kernel via `kevy_sub_wait` (a spin-poll build burned ~one core).

## Measured (iOS Simulator, iPhone 17 Pro / iOS 26.5, arm64, Release)

| | Expo door | Nitro door |
|---|---:|---:|
| `cmd` PING | ~179k/s | ~1.89M/s (**10.5×**) |
| `cmd` SET | ~173k/s | ~1.64M/s (9.5×) |
| pub/sub poll | — | ~1.06M/s |
| pub/sub batched push | — | ~1.39M/s (1.3× poll) |

`abi()` pure-JSI ~20M/s. Idle CPU with a live push subscription: **0.0%**
(the `kevy-push-poll` thread parks in `kevy_sub_wait`, same as Android). All
50001/50000 pub/sub frames delivered; `abi=1`, `cmd(PING)="+PONG\r\n"`.
(Release numbers run higher than Android's Debug numbers; the door-vs-door
ratios are the story and hold on both platforms — batched push = 1.3× poll,
per-message push < poll.)

See `bench/pubsubgate/LEDGER.md` in the kevy repo for the full method and
raw numbers.

## Build

Android: the engine ships as a prebuilt per-ABI `libkevy_ffi.so` under
`android/src/main/jniLibs/` (rebuild with
`packaging/android/build-ffi-jnilibs.sh`).

iOS: the engine ships as `ios/KevyEngine.xcframework` (built by
`packaging/apple/build-xcframework.sh`; gitignored for size — regenerate
locally). `KevyNitro.podspec` compiles the C++ HybridObject and links it.
The sim slice is `ios-arm64-simulator`; a Release sim build needs
`ARCHS=arm64 EXCLUDED_ARCHS=x86_64` (Apple-Silicon host) or an added
x86_64-sim slice.

Regenerate the Nitro bindings with `npm run specs` (nitrogen).

## Gate

The Nitro door is a JSI C++ HybridObject — it only exists inside a running
React Native app, so it is gated **on a device**, not on the host. Its
smoke lives in the shared Expo example app: `App.tsx` calls
`runNitroBench()` (from `bindings/expo/example/nitroBench.ts`), which opens
a Nitro db, runs `cmd`/pub-sub/batched-push against it, and logs its
verdict lines to the device console — `NITROGATE:ERROR …` on any failure,
the door-vs-door bench lines otherwise (see the tables above).

Drive it with the existing mobile gate, which boots the example app onto a
simulator/emulator and reads the device log:

```
bash bench/mobilegate.sh expo ios       # iOS Simulator (Xcode)
bash bench/mobilegate.sh expo android    # Android emulator (SDK)
```

Because a native build + device boot is heavy and toolchain-bound, this is
a **developer / CI-on-macOS gate**, not part of the per-push matrix — the
same status as every other mobile door. It cannot run on a host without a
booted simulator/emulator. Perf method and raw numbers:
`bench/pubsubgate/LEDGER.md`.

(The sibling raw JVM/JNI door, `bindings/android`, is pure-JVM and *is*
host-runnable — see `bench/jnigate.sh`.)

## License

Apache-2.0 OR MIT.
