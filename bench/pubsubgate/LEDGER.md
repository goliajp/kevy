# pubsubgate ledger — kevy pub/sub vs mitt (in expo React Native)

The bar for kevy-as-an-event-bus on mobile is **mitt** — a ~200-byte
in-process JS emitter that many expo/RN apps already use. This ledger
records the head-to-head **in the Hermes runtime on device**, losing axes
named not hidden.

The comparison is deliberately unfair in mitt's favour and that's the
point: mitt is a same-thread synchronous function call — zero
serialization, zero boundary. kevy pub/sub crosses **JS↔native** (Expo
module dispatch → packed-argv marshalling → JNI/FFI → RESP frame → channel
match) on *every* publish and *every* drain. So mitt wins raw dispatch by
construction. What we measure is the **crossing tax** — how much a
JSI/Nitro fast-path door could buy back — and whether kevy pub/sub is
already fast enough to be an event bus where you want what mitt can't
give: pattern subscribe, decoupled/multi subscribers, cross-JS-context
delivery, and the same handle is your KV store.

## Baseline — Android emulator (x86_64), Hermes, expo debug build

`bindings/expo/example/pubsubBench.ts`, 50 000 publishes of a fixed
payload through one `subscribeRaw` subscriber, drained per publish.
Measured on-device, logged as `PUBSUBGATE:` lines the gate reads.

| Axis        | mitt          | kevy pub/sub | mitt/kevy | kevy detail (recv / ms) |
|-------------|--------------:|-------------:|----------:|-------------------------|
| 16 B        | 4,545,455/s   | 49,214/s     | **92.4×** | 50001/50000 in 1016 ms  |
| 256 B       | 5,555,556/s   | 52,302/s     | **106.2×**| 50001/50000 in 956 ms   |

Correctness: kevy delivered **every** message (recv == n) — the throughput
is real, not dropped-message inflation.

### Reading it

- **kevy pub/sub ≈ 50 k msg/s on device**, flat across payload size (16 B
  vs 256 B barely moves it) — so the cost is *per-call*, not per-byte:
  the crossing dominates, the payload copy is noise. That is the signature
  of a boundary tax, not a data tax.
- **~20 µs per publish+drain cycle.** Each cycle is ~3 JS↔native crossings
  (1 `publish` + the drain's `subNext` that returns the message + the
  `subNext` that returns empty to end the loop). ≈ 6–7 µs per crossing —
  the Expo-module dispatch + packed-argv (allocate a `Uint8Array`, u32
  length prefixes) + JNI + RESP encode/decode, paid every call.
- **mitt is ~90–106× faster** and always will be *something*×: it never
  leaves JS. The question is not "beat mitt" (impossible for anything that
  crosses a boundary) but "how close can we get, and is 50 k/s enough."
  For typical UI/app event rates 50 k/s is ample; for high-fanout or
  high-frequency streams it is the ceiling to attack.

## Attack — Nitro/JSI fast-path door (planned)

The tax is the crossing, not the engine. `react-native-nitro-modules`
(JSI + C++ codegen) replaces the Expo-module dispatch + packed-argv
marshalling with **direct JSI calls** into a C++ HybridObject that calls
`kevy-ffi` (the prebuilt C ABI — no cargo, the Nitro C++ links the
vendored lib), passing payloads as `ArrayBuffer` by reference.

Two wins to measure:
1. **Per-call cost**: JSI HostFunction dispatch is ~100–500 ns vs the
   ~6–7 µs measured here — a 10–50× cut on each crossing.
2. **Push instead of poll/drain**: JSI lets native invoke a JS callback
   directly, so a subscriber receives each message in *one* native→JS
   call instead of the publish + 2× `subNext` drain. That removes ~2 of
   the 3 crossings per message.

Ceiling target: from ~50 k/s toward several hundred k/s — closing the 92×
to roughly 10× (mitt's zero-crossing floor is unbeatable, but 10× is a
different product than 92×). The Nitro door also lifts **every** RN kevy
op (GET/SET call overhead), not just pub/sub — same crossing, same tax.

## Attack — Nitro/JSI door, measured (feasibility spike)

Built as `bindings/nitro-spike` (`react-native-kevy-nitro`): a C++-only
Nitro HybridObject calling `kevy-ffi` directly via JSI — `abi()`,
`cmd(argv: ArrayBuffer): ArrayBuffer`, and a poll-model pub/sub trio. Same
emulator (arm64-v8a), Hermes, expo debug, new arch. `bindings/expo/example/
nitroBench.ts` times identical round-trips through both doors; NITROGATE
lines from logcat, median of 2 on-device runs.

| Axis            | Expo door | Nitro door | Nitro/Expo |
|-----------------|----------:|-----------:|-----------:|
| `abi()` pure-JSI | —        | 9,090,909/s | — (≈110 ns/call) |
| `cmd` PING       | 143,266/s | 487,805/s  | **3.4×** |
| `cmd` SET        | 139,082/s | 295,858/s  | **2.1×** |
| pub/sub 16 B     | 48,734/s  | 485,447/s  | **~10×** |

Correctness: `NITROGATE:SMOKE abi=1 ping="+PONG\r\n"` — the C++ door really
reaches the engine, and pub/sub delivered every message (50001/50000).

### Reading it

- **Pub/sub 48 k → 485 k/s (~10×)** — exactly the LEDGER's prediction. The
  gap to mitt collapses from **93× to 8.6×**: a different product. And this
  is *still the poll/drain shape* (publish + `subNext` loop, ~2 crossings/
  msg); a true native→JS push callback would remove one more crossing.
- **`cmd` PING 143 k → 488 k/s (3.4×)** — a single cmd is one crossing, so
  the Expo door's 143 k cmd/s ≈ 7 µs/crossing (matches the pub/sub-derived
  6–7 µs above). Nitro's ~2 µs/cmd is *not* the ~110 ns JSI floor because
  `cmd` still allocates+copies a reply ArrayBuffer and runs the RESP
  encode; the crossing is cheap now, the marshalling is what's left.
- **`abi()` at 9 M/s (~110 ns)** confirms the JSI HostFunction dispatch cost
  the hypothesis assumed — two orders of magnitude under the Expo door.

Viability: nitrogen 0.36.1 codegen + the native C++ build both work on RN
0.86.0 (new arch). Two RN-0.86 frictions surfaced and were solved in the
spike, not worked around: (1) a pure-C++ Nitro module needs a Kotlin
`ReactPackage` shim for Expo autolinking to link it and to load the .so;
(2) `libkevy_ffi.so` needed an explicit SONAME or the linked-in path breaks
dlopen. Both are one-liners, now in the tree.

Next (not done here): a native→JS **push** callback path (removes the drain
crossing) and an iOS run.
