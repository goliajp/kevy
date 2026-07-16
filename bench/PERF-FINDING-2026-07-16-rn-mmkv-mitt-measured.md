# PERF FINDING — RN device measurement REFUTES the decomposition estimate

**Date:** 2026-07-16 · **Env:** iOS Simulator (iPhone 17 Pro, iOS 26.5,
Apple-Silicon host), Debug + Metro, react-native-kevy-nitro (Nitro/JSI) vs
react-native-mmkv 3.3.3 (also Nitro/JSI) + mitt. 3 stable runs, N=100k.

**Verdict:** on a real RN runtime, through the Nitro door, **kevy LOSES to
MMKV on KV and to mitt on pub/sub** — the opposite of what the
`PERF-DECOMP-2026-07-16` estimates predicted. The scalar door + raw pub/sub
drain are real *internal* wins (measured), but they do **not** close the gap
to the external bars. This is the methodology's core lesson (decomposition
estimates get refuted by real measurement) in action — recorded honestly, no
spin.

## Measured (3 runs, median, 16 B values)

| Axis | kevy (Nitro) | MMKV / mitt | kevy/bar | Verdict |
|------|-------------:|------------:|---------:|---------|
| KV GET | ~704k ops/s (~1.4 µs) | MMKV ~1.61M (~0.62 µs) | **0.4×** | **MMKV ~2.3× faster** |
| KV SET | ~794k ops/s (~1.26 µs) | MMKV ~1.39M (~0.72 µs) | **0.6×** | **MMKV ~1.7× faster** |
| pub/sub batched | ~445k ops/s | mitt ~5.5M | **0.08×** | **mitt ~12.5× faster** |

Internal wins that DID land (real, stable):
- `getData/setData` scalar door vs the `cmd`(RESP) lane: **1.5× GET / 1.8× SET**
  — the RESP-tax removal works, just isn't enough to beat MMKV.
- `cmd` Nitro door vs Expo door: **4.0× PING / 3.4× SET** — the JSI door beats
  the Expo-module door as designed.
- `abi()` pure-JSI: ~10M ops/s (the JSI floor).
- pub/sub `pushBatched/poll ≈ 1.0×` (NOT the estimated 1.3×) — the batch only
  amortized **~3 frames/hop** on this publish-one-at-a-time workload, so the
  CallInvoker hop barely amortizes; `push/poll = 0.6×` (per-message loses,
  confirmed).

## Why the estimate was refuted

The decomposition estimated `getData` **~120–200 ns** (beating MMKV ~425 ns)
by counting one JSI hop (~50 ns) + `kevy_get` (~30 ns FFI, measured) + one
copy. The **real** per-op is **~1.4 µs** — an order of magnitude over the
estimate. The missing ~1.2 µs is the **Nitro binding layer**: ArrayBuffer
creation/marshalling per call, HostObject method dispatch, the nitrogen glue,
`takeBuf`. The decomposition measured the FFI boundary (correctly — 20–35 ns)
but estimated the JSI crossing at the bare `abi()` cost and ignored the
ArrayBuffer marshalling, which dominates.

Crucially, **MMKV 3.3.3 is ALSO a Nitro module** — same JSI floor, same
runtime. So kevy has *no* crossing advantage here; MMKV's binding is simply
~2× leaner per op (~0.62 µs vs ~1.4 µs). The gap is not the engine (kevy's
FFI scalar is 20–35 ns, faster than MMKV's mmap) and not the crossing (shared)
— it is **kevy's Nitro binding overhead vs MMKV's**.

## The real next attack surface (if pursued)

Not the engine, not the FFI, not the RESP tax (all already addressed). The
open gap is the **Nitro glue**: why is kevy's `getData` ~1.4 µs when MMKV's
`getBuffer` is ~0.62 µs, both one JSI hop + one buffer? Candidates to
decompose (a NEW decomposition, targeting the binding, not the engine):
per-call `ArrayBuffer::move` cost, key/value ArrayBuffer construction on the
JS side, the `std::vector`→ArrayBuffer copy in `takeBuf`, HostObject dispatch
overhead vs MMKV's. Until that is decomposed on-device, any further estimate
is a guess — this finding refuted the last one.

For pub/sub, mitt's zero-crossing floor (~5.5M) is unbeatable by anything that
crosses a thread; the realistic question is whether larger real-world batches
(bursty publishers) amortize the hop better than this synthetic
one-at-a-time loop's ~3 frames/hop. The raw+packed drain is correct
infrastructure but the amortization is workload-bound.

## Reproduction (build issues found + fixed to get the app running)

Getting the Nitro door to build + run in the expo example surfaced real,
previously-unexercised integration gaps (the Nitro door had never been built
into an app that also carries the Expo door). All fixed to reach the numbers:

1. **`KevyEngine.xcframework` recipe** — not produced by any script; it is
   `packaging/apple/build-xcframework.sh`'s `Kevy.xcframework` (static),
   renamed. Confirmed by the KevyNitro.podspec comment.
2. **`redefinition of module 'Kevy'`** — the Expo door's `Kevy.xcframework`
   and the Nitro door's engine BOTH defined `module Kevy`; in one app that
   collides and cascades to "could not build module 'Foundation'". Fix: the
   Nitro engine's module renamed to `KevyEngine` (its C++ `#include`s the
   header + links symbols; it never `@import`s, so it needs no `Kevy` module).
   **This is a real repo bug for the v4 ship** — the Nitro engine artifact
   must not ship a `module Kevy`.
3. **Stale Expo engine** — `bindings/expo/ios/Kevy.xcframework` predated the
   `kevy_sub_next_raw`/`_wait_raw` symbols; since it (not the pitfall-affected
   Nitro static xcframework) is what actually links, the raw symbols were
   `Undefined`. Fix: rebuild the engine from current kevy-ffi.
4. **Metro can't resolve the symlinked doors' peer deps** (`react-native-nitro
   -modules` from `../../nitro`). Fix: `metro.config.js` with `watchFolders` +
   `resolver.nodeModulesPaths` → the app's node_modules (committed).

## Honesty ledger

- This is a **simulator**, not a device; single-digit-ms Date.now() timing;
  3 runs stable but not a variance study. The relative standing (MMKV/mitt
  faster) is a strong, consistent signal; absolute ns are sim-inflated.
- No number here is estimated — all six ratios are measured on-device, 3 runs.
- The decomposition's FFI-level measurements (20–35 ns scalar, 208 ns
  encode_frame) remain correct; what was refuted is the *device-level
  projection through the JSI/Nitro layer*.
