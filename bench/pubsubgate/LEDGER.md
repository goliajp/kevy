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

Next: build the Nitro door (spec + C++ glue to kevy-ffi + push callback),
re-run this gate, record the post-attack row here.
