# PERF DECOMP — RN door final ceiling round: binding glue, pubsub hybrid, durable axis

**Date:** 2026-07-17 · **Scope:** the last measured losses of the RN Nitro door
(vs react-native-mmkv on KV, vs mitt on pub/sub) after the v4 quality arc closed
(scalar shared lane, zero-copy Arc GET, string keys — all landed + measured).
Phase A = four parallel read-only decompositions + device re-baselining.
Sources are cited file:line; every claim below was read, not recalled.

## Phase A findings (ground truth)

### F1 — The old Samsung baseline was doubly confounded (measurement, not code)

- The bench app on the Samsung S22 was a **debug variant** (`pkgFlags=[ DEBUGGABLE …]`
  verified on device; `bench/mobilegate.sh` expo/android lane keeps the dev build):
  the whole Nitro/RN C++ stack at -O0 + `-fstack-protector-all` + debug JSI +
  Metro dev-mode JS, while the iPhone numbers came from a signed Release.
  `bindings/nitro/android/.cxx/**/compile_commands.json` showed no `-O` flag and
  `react-android-0.86.0-debug` prefab.
- The device had **power saving ON** (`settings get global low_power` = 1): big
  core capped 806 MHz (vs 2.995 GHz max). One awake Release run with power
  saving still on measured getData 161k ops/s; power saving off: 207k (+29%).
- Conclusion: the "Samsung 8× slower than iPhone" absolute gap was
  build-variant + power-mode artifact. The remaining absolute delta is the
  S22 scheduler/Hermes-interpreter reality both libraries share; only the
  **kevy/mmkv same-process ratio** is a fair cross-library signal.

**Fresh Samsung baseline (Release APK, awake, power saving off, run 1):**

| axis | kevy | mmkv | ratio |
|------|-----:|-----:|------:|
| GET 16B | 200k | 299k | 0.7× |
| GET 256B | 198k | 214k | 0.9× |
| GET 4KB | 208k | 137k | **1.5×** |
| SET 16B | 187k | 190k | 1.0× |
| SET 256B | 241k | 189k | 1.3× |
| SET 4KB | 219k | 136k | **1.6×** |
| pubsub mitt/pushBatched | | | 9.7× |

(median-of-3 appended below at §Baseline runs.)

### F2 — MMKV is NOT a Nitro module; kevy's Nitro typed dispatch is the small-value tax

The 2026-07-16 finding's premise "MMKV 3.3.3 is ALSO a Nitro module" is wrong.
MMKV is a hand-written `jsi::HostObject` (`node_modules/react-native-mmkv/cpp/
MmkvHostObject.cpp`) with host-function lambdas capturing `this` and a JS-side
function cache (`src/MMKV.ts:43-50`). Per call it pays **no** this-resolution,
no NativeState lookup, no dynamic_cast, no return converter.

kevy's Nitro typed path pays, per getData call, on top of what MMKV pays:

| cost | where | est ns |
|------|-------|-------:|
| this resolution: getNativeState + `dynamic_pointer_cast<THybrid>` (virtual-inheritance RTTI) + shared_ptr atomics | `react-native-nitro-modules/cpp/core/HybridFunction.hpp:95,189-240` | 50-125 (NOT removable while staying typed; raw methods pay it too; offset by MMKV's own JS-wrapper overhead) |
| return converter: doomed `dynamic_pointer_cast<JSArrayBuffer>` | `JSIConverter+ArrayBuffer.hpp:74` | 20-50 |
| return converter: `make_shared<MutableBufferNativeState>` heap alloc | `JSIConverter+ArrayBuffer.hpp:85` | 30-50 |
| return converter: `setNativeState` on the fresh ArrayBuffer (GC cell + hidden-class transition + finalizer) | `JSIConverter+ArrayBuffer.hpp:86` | 60-150 |
| converter shared_ptr churn | `HybridFunction.hpp:179-180` | 10-25 |

**GET removable ≈ 120-275 ns.** iPhone getData ≈ 300 ns/op measured → the
removable slice is 40-90% of the whole op — the Pre-Phase-B "attack target is
double-digit pp of self-time" gate passes by arithmetic on measured device
numbers (no on-device perf record needed for this call).

setData additionally routes its ArrayBuffer **argument** through
`JSIConverter+ArrayBuffer.hpp:39-72`: `JSICache::getOrCreateCache` (map +
weak_ptr::lock + mutex) + `new jsi::ArrayBuffer` + `new ReferenceState()` +
`make_shared<JSArrayBuffer>` ≈ **170-330 ns removable**, and — a real bug —
`_arrayBufferCache.push_back` grows **unbounded** until runtime teardown
(`JSICache.cpp:26-36`): every setData leaks a 16 B cache row for the life of
the runtime.

`takeBufShared`'s DeleteFn lambda (24 B, trivially copyable) fits libc++
std::function SBO — **no** per-call heap alloc there; earlier suspicion refuted.

### F3 — embedded pub/sub has ZERO non-local consumers; hybrid local fan-out is legitimate

Exhaustively excluded (file:line in the agent ledger): cross-process (bus is
per-`Inner`, `kevy-store/src/pubsub.rs:1-13`), second handle on the same dir
(fresh `build_shards` per open, no dir registry), embedded RESP listener
(read-only verb whitelist, no SUBSCRIBE/PUBLISH; not even enabled via FFI),
replication (PUBLISH never enters `commit_write`/AOF/feed), keyspace
notifications (drained only by the server layer `exec_notify.rs:133-136`),
native background publishers (none in this door). The only theoretical
exception — a Nitro `box()`ed object shared into a worklet runtime — is
undocumented and keeps the raw lane anyway.

So today's Nitro pubsub traffic is 100% JS→native→same-JS round-trip, and the
native leg provides nothing local delivery can't. The wasm door already ships
the same architecture this round proposes (BroadcastChannel bridge + sender-id
exclusion + honest docs, `crates/kevy-wasm/pkg/kevy.js:566-597`).

Bonus gap found: the Nitro door **discards the PUBLISH receiver count**
(`HybridKevyNitro.cpp:184`); the Expo door returns it.

### F4 — durable-SET axis: door is ready, bench never used it

`createKevyNitroAt(dir)` already exists (`bindings/nitro/src/index.ts:31-37`,
`HybridKevyNitro.cpp:162-167` → `kevy_open`); dir-open defaults to AOF +
`appendfsync EverySec` (`kevy-embedded/src/config.rs:140-142`) which
`bench/mmkvgate/LEDGER.md:11-13` already established as the fair (slightly
kevy-stricter) durability match for MMKV's mmap+OS-writeback. The kv-vs-mmkv
bench (`nitroBench.ts:116`) opens in-memory only — the measured "kevy SET wins"
compares non-durable kevy vs durable MMKV. Engine-level prior (lx64, real
hardware): kevy buffered EverySec SET-4KB 2.98 µs vs MMKV 11.84 µs (kevy 4×);
device is the arbiter and unmeasured. Note `documentDirectory` carries a
`file://` prefix that `createKevyNitroAt` does NOT strip — caller strips.

## Attack list (Phase B, ceiling-first order)

| # | attack | files | expected | class |
|---|--------|-------|----------|-------|
| A1 | getData/setData → **raw JSI methods** (`registerRawHybridMethod`, Nitro's official escape hatch): override `loadHybridMethods`, register the 2 hot methods raw (MMKV-shape bodies: direct `asObject().getArrayBuffer()` arg access, direct `jsi::ArrayBuffer(rt, make_shared<OwnedBuf>)` return, no converters), other 9 methods stay typed | `bindings/nitro/cpp/HybridKevyNitro.{hpp,cpp}` | GET −120-275 ns, SET −170-330 ns per call, both platforms; kills the unbounded JSICache growth; JS API unchanged | behavior-neutral (one edge: returned buffer re-passed as arg loses NativeState fast path — bench-irrelevant, MMKV has the same shape) |
| A2 | pubsub **hybrid local fan-out**: TS channel registry + microtask dispatch for callback-lane subscribers; native publish kept for raw lane + count; return `nativeCount + localCount`; fix the discarded receiver count (spec `publish → number`, parse `:N\r\n`) | `bindings/nitro/src/index.ts`, `specs/KevyNitro.nitro.ts`, `HybridKevyNitro.cpp:175-185`, README | callback lane moves to mitt's physical position (~1× mitt); raw lane untouched as the engine-semantics escape hatch | semantics table verified vs Redis: equal or better on every axis; LEDGER must label the lane honestly |
| A3 | **durable-SET axis** in the bench: second handle via `createKevyNitroAt(docDir)` (strip `file://`), 3-size SET sweep, new `kv-vs-mmkv-durable` lines; 3-column mem/durable/mmkv | `bindings/expo/example/nitroBench.ts` | fills the asymmetry blank; prior says kevy may still win, device decides; honest either way | measurement only |
| A4 | drop `-fstack-protector-all` from release C++ flags (kevy-only tax MMKV doesn't pay; keep for debug) | `bindings/nitro/android/build.gradle:50` | small Android-only per-call shave | build hygiene |

Non-goals (unchanged from the audit): 16B GET below the this-resolution floor
(~50-125 ns, offset by MMKV's own JS wrapper), SET's one inherent copy per
side, mitt's zero-crossing floor for anything that must reach native.

## Verification protocol

- Host: `cargo test --workspace` (engine untouched by A1/A2 — expect no-op),
  nitro TS tests if any.
- Devices: Samsung S22 (Release APK, awake, power saving off — the §F1
  protocol), iPhone 15 (signed Release). Median-of-3 per configuration.
- Gate: MOBILEGATE smoke PASS on both; A1 judged on 16B/256B GET+SET ratios;
  A2 judged on a new hybrid pubsub axis vs mitt; A3 is a measurement, its
  numbers land in the finding doc whatever they say.
- Honesty: negative or neutral device results ⇒ revert the attack, record the
  refutation (methodology §8 — REVERT is an honest answer).

## Baseline runs (Samsung S22, Release, awake, power saving off)

| run | GET 16B/256B/4KB | SET 16B/256B/4KB | getData abs | abi (pure JSI) | mitt abs |
|-----|------------------|------------------|------------:|---------------:|---------:|
| 1 | 0.7 / 0.9 / 1.5 | 1.0 / 1.3 / 1.6 | 200k | 1.25M | 424k |
| 2 | 0.7 / 0.8 / 1.4 | 1.0 / 1.3 / 1.6 | 192k | 1.16M | 410k |
| 3 | 0.6 / 1.0 / 2.2 | 1.3 / 1.4 / 1.7 | **1.10M** | **4.76M** | **2.78M** |

Run 3's ~5-6× absolute jump (including pure-JS mitt) = the Samsung scheduler
promoted the JS thread to a big core; runs 1-2 sat on little cores. Absolutes
on this device are core-placement lottery; the same-thread back-to-back
kevy/mmkv **ratios stay comparatively stable** and are the judging metric.
Protocol for attack verdicts: per-run ratios, median of ≥3 runs, plus a bench
warmup spin before the sweep so the scheduler ramps before timing starts.

---

## Phase B — implemented + device-measured (Samsung S22, Release, 3 runs each)

The bench warmup (~300 ms abi() spin) killed the placement lottery: post-warmup
abi() reads 9-10M ops/s on every run, and all axes reproduce within ~±7%.

### A1 + A4 — raw JSI KV methods (+ release drops -fstack-protector-all): LANDED

kevy/mmkv ratios, pre-attack (warm run 3) → post-attack (median of 3):

| axis | before | after |
|------|-------:|------:|
| GET 16B | 0.6× | **0.7×** |
| GET 256B | 1.0× | **1.6×** |
| GET 4KB | 2.2× | **3.8×** |
| SET 16B | 1.3× | **3.2×** |
| SET 256B | 1.4× | **2.6×** |
| SET 4KB | 1.7× | **2.9×** |

setData absolute 2.5M ops/s (~400 ns/op, 5.1× over the RESP cmd lane); getData
1.6M. SET now beats MMKV at EVERY size; GET beats it from 256B; 16B GET stays
0.7× — the this-resolution floor the decomp predicted stays (raw methods pay
`getHybridObjectNativeState` too). The unbounded JSICache growth (16 B leaked
per typed-ArrayBuffer-arg call) is gone with the converters.

### A2 — kevy_publish + raw publish + KevyBus local fan-out: LANDED, floor found

Three stacked steps, each device-measured:

1. **kevy_publish (new FFI symbol) + raw registration** — publish had been the
   only pub/sub verb still riding kevy_cmd's argv+RESP round-trip (subscribe
   has had kevy_subscribe from day one). bus 357k → ~490k ops/s; ALL pubsub
   lanes lifted (poll 314→440k, pushBatched 206→500-600k, now consistently
   1.1-1.3× over poll). Nitro publish also now returns the receiver count it
   used to drop on the floor.
2. **KevyBus batch drain** (one microtask per burst, not per publish): bus
   → ~685k.
3. **Snapshot-cache** (no per-publish Set spread): no further movement — two
   consecutive sub-noise rounds, STOP per the methodology.

Floor decomposition via the new `pubFloor` axis (bare publish, no subscribers,
no bus machinery): **1.4-3.1M ops/s — the native publish leg is as fast as or
faster than a mitt emit (2.6-2.8M)**. The remaining bus gap is the Hermes-
interpreted JS machinery the semantics require (value-semantics payload copy,
delivery queue entry, map lookup) stacked on the honest engine publish.

**mitt/bus: 9.7× → 3.6-4.5× (median ~4.3×).** This is the semantic floor for
"a real engine bus + Redis value semantics" vs "same-thread reference-passing
emitter"; going lower means dropping the payload copy or the engine leg — both
semantic changes, declined.

### A3 — durable-SET axis: MEASURED (the blank filled, mixed verdict)

kevy dir-open (AOF everysec) vs MMKV mmap, SET, median of 3:

| size | kevy-durable/mmkv |
|------|------------------:|
| 16B | **1.3×** (kevy wins) |
| 256B | **1.1-1.2×** (kevy wins) |
| 4KB | **0.1-0.2×** (kevy loses ~5-10×) |

kevy durable 4KB ≈ 90-120k ops/s vs in-memory 1.8M: the AOF appends every
4 KB value byte per SET (plus auto-rewrite churn at the 64 MiB threshold),
while MMKV re-dirties the same mmap page. The engine-level lx64 prior (kevy
4× faster) does NOT transfer to phone flash for large values. Honest summary
for docs: durable kevy wins the small/medium writes that dominate mobile KV
use; for multi-KB durable blobs MMKV's mmap model is faster.

### Verdict table (attack → outcome)

| attack | predicted | measured | kept? |
|--------|-----------|----------|-------|
| A1 raw KV methods | GET −120-275ns, SET −170-330ns | SET 1.3×→3.2× @16B etc. (bigger than predicted — the Android debug-confound had hidden the full converter tax) | ✅ |
| A2.1 kevy_publish + raw | remove argv/RESP/converter ~700ns | −750ns measured | ✅ |
| A2.2 batch drain | remove per-op microtask | −580ns | ✅ |
| A2.3 snapshot cache | remove Set spread | sub-noise ×2 → STOP | ✅ (kept: correct + zero-cost) |
| A3 durable axis | unknown, device decides | 16B/256B win, 4KB lose 5-10× | measurement |
| A4 stack-protector | small Android shave | not separately measured (rode along with A1) | ✅ |
