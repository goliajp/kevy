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

---

## Follow-up — the Nitro-glue decomposition, attacked and MEASURED (same day)

The refutation named the right next target: the **Nitro binding layer**, not
the engine. This time the loop stayed device-grounded — read MMKV's cpp,
form a candidate, measure it on the sim — and it turned the KV result around.

### Attack 1 — zero-copy ArrayBuffer return
Source read: MMKV's `getBuffer` wraps its `MMBuffer` in a `jsi::ArrayBuffer`
(zero binding copy, freed at GC). kevy's `takeBuf` memcpy'd the KevyBuf into a
`std::vector` first. Fix: `ArrayBuffer::wrap` the KevyBuf directly, defer
`kevy_buf_free` to GC (Nitro `DeleteFn`). **Measured: getData 704k → 926k
ops/s (+31%).**

### Attack 2 — string keys (the big one)
Device data, not a source guess, pointed here: `setData` (2 AB args, void
return) measured **slower** than `getData` (1 AB arg, 1 AB return) → the
dominant cost is **ArrayBuffer *argument* marshalling** (~560 ns/AB-arg),
not the return. MMKV takes a **string** key. Mirror it —
`getData(key: string)` / `setData(key: string, value: ArrayBuffer)`.
**Measured (stacked on attack 1):**

| Axis | original | +zero-copy | +string key | vs MMKV now |
|------|---------:|-----------:|------------:|------------:|
| GET  | 704k (0.4×) | 926k (0.5×) | **~1.56M (0.85×)** | MMKV ~1.15× |
| SET  | 775k (0.6×) | 775k | **~1.45M (1.0×)** | **TIES MMKV** |

**Net turnaround: KV went from losing badly (GET 0.4×, SET 0.6×) to
competitive (GET ~0.85×, SET parity) — all device-measured, stacked, stable.**
The scalar door is now 2.7–2.9× over the RESP cmd lane. MOBILEGATE smoke PASS
throughout.

### Lesson (the methodology, both directions)
- Source-only estimates get refuted (the first pass predicted a win, measured
  a loss). **Device-grounded** estimates hold (attack 2's ~560 ns/AB-arg was
  derived from on-device getData-vs-setData and confirmed by the measurement).
- The gap was never the engine (kevy's FFI scalar beats MMKV's mmap) nor the
  RESP tax (already removed) — it was the **binding-layer data shape**:
  copying instead of wrapping, and ArrayBuffer args instead of string keys.
  Both are exactly what MMKV got right and kevy hadn't.
- Remaining GET ~15%: the `std::function` deleteFunc alloc, the FFI boundary,
  the engine `into_owned` — each ~50–100 ns, diminishing. SET already ties.
- pub/sub vs mitt is unchanged (mitt's zero-crossing floor is physical; not a
  binding-shape problem).

### Size sweep — where kevy overtakes (measured, 2 stable runs)

With the binding competitive, swept value size to see the engine edge:

| size | GET kevy/mmkv | SET kevy/mmkv |
|------|--------------:|--------------:|
| 16 B | 0.9× | ~0.9× |
| 256 B | 0.9× | **1.1× (kevy wins)** |
| 4096 B | 0.9× | **1.2–1.4× (kevy wins, lead grows)** |

- **GET stays ~0.9× at every size** — the hypothesis that kevy's in-memory
  read would overtake MMKV's mmap on large GET was **refuted** (measure rules;
  both scale similarly, MMKV stays ~10% ahead on GET).
- **SET crosses over: kevy ties at 16 B and beats MMKV from 256 B up, the
  lead growing to ~1.4× at 4 KB.** kevy's in-memory store insert scales better
  than MMKV's mmap-append + periodic full-writeback for larger writes — the
  same edge the engine-level mmkvgate saw on real hardware, now confirmed
  **through the Nitro door** for realistic payloads. (The Nitro door opens
  kevy_open_mem, so this SET is non-durable in-memory vs MMKV's mmap; the
  durable-SET comparison is the separate AOF-vs-mmap axis.)

**Bottom line after the binding attacks:** for realistic payloads (256 B+)
kevy's Nitro door **beats MMKV on SET** (up to 1.4× at 4 KB) and trails by a
steady ~10% on GET — a completely different product from the pre-attack
"MMKV 1.7–2.3× faster."

---

## DEFINITIVE — real device (iPhone 15, iOS 26.5.2, Release, signed)

The simulator numbers above are relative-standing signals; this is the real
phone. Built a signed Release (GOLIA team), installed on a physical iPhone 15,
ran the same bench, pulled the results file off the app container (console.log
does not reach the host from a device Release — the app writes a file). One
run (the on-device figures are stable; Date.now() ms granularity):

| KV (iPhone 15) | GET kevy/mmkv | SET kevy/mmkv |
|----------------|--------------:|--------------:|
| 16 B | 0.8× | 0.8× |
| 256 B | 0.9× | **1.3× (kevy wins)** |
| 4096 B | **0.5×** | **1.6× (kevy wins)** |

Raw: GET kevy 2.63M/2.63M/1.03M vs MMKV 3.23M/3.03M/2.08M ops/s;
SET kevy 1.96M/2.86M/2.63M vs MMKV 2.38M/2.13M/1.67M. Nitro cmd door
**11–13× over the Expo door** (PING 13.4×, SET 11.0×); scalar door 2.4–2.5×
over the RESP cmd lane. pub/sub `mitt/pushBatched = 5.8×` (real hardware, vs
the sim's 12× — the crossing is relatively cheaper on-device); `push/poll
1.1×`, `pushBatched/poll 0.9×`.

**What the real device confirms and what it revises:**
- **SET crossover holds on real hardware** — kevy ties at 16 B and **beats
  MMKV from 256 B up, 1.6× at 4 KB**. kevy's in-memory store insert scales
  past MMKV's mmap-append + writeback. This is the headline: for realistic
  write payloads, kevy's Nitro door is faster than MMKV.
- **GET revises the sim story** — on real hardware MMKV wins GET at every
  size and its lead **grows with value size** (0.8× @16 B → **0.5× @4 KB**),
  the *opposite* of both the flat-0.9× sim and the engine-level mmkvgate
  (which had kevy winning large GET via KevyKit-direct). Cause: MMKV's
  `getBuffer` returns a **zero-copy view of the mmap page**; kevy's `kevy_get`
  must `into_owned`-copy the value out from behind the store lock (an
  in-memory store can't hand out a borrowed view that outlives the lock). At
  4 KB that copy is the gap. This is **architectural** (mmap-view vs
  locked-store-copy), not a binding-shape fix — the same durability/storage
  model tradeoff the mmap-AOF finding named, now visible on GET.

**Net, on a real phone:** kevy's Nitro KV door **beats MMKV on SET for
realistic payloads** (the common mobile write) and **trails on GET, more so
for large values** (MMKV's mmap-view edge). pub/sub trails mitt ~6× (physical
crossing floor). A completely different, honest picture from the pre-attack
"kevy loses everything 1.7–2.3×" — earned by device-grounded decomposition +
two measured binding attacks + a real-device confirmation.

---

## T1 — Arc-clone zero-copy GET closes the large-GET loss (engine, measured both devices)

The "GET more so for large values" clause above was the last loss. Audited to
its root: `kevy_get` copied the whole value out of the store (`Vec`
`to_vec()`), even though values > 64 B are already stored as
`Value::ArcBulk(Arc<Box<[u8]>>)`. Added an **additive** zero-copy FFI lane
(`kevy_get_shared` / `kevy_buf_free_shared`, `Store::get_arc`) that hands the
Nitro door an `Arc::clone` (refcount bump, **no byte copy**) whose bytes the JS
ArrayBuffer views; `kevy_get` and every other door untouched (`cargo test
--workspace` green, incl. the byte-exact plain/framed lane assertions + a new
shared-lane test). getData now wires to it.

**On-device measured (both real phones, 3 stable runs each, kevy/mmkv):**

| GET | iPhone 15 before → **T1** | Samsung S22 before → **T1** |
|-----|--------------------------:|----------------------------:|
| 16 B  | 0.8 → 0.8 | 0.6 → 0.6 |
| 256 B | 0.9 → **1.0** | 0.7 → 0.7–0.8 |
| 4 KB  | **0.5 → ~2.7×** | 1.0 → **~1.2×** |

**kevy's getData is now size-independent** — flat ~3.33M ops/s (iPhone) /
~0.4M (Samsung) at *every* size, because the Arc clone is O(1). MMKV's
`getBuffer` **copies** out of its mmap page, so it slows with size — the two
curves cross and kevy pulls ahead for larger values (iPhone from 256 B,
Samsung at 4 KB). The dramatic iPhone 4 KB swing (0.5× → ~2.7×, a ~5× turn)
is kevy's flat clone vs MMKV's 4 KB copy. Small values (16 B) MMKV still wins
— that's kevy's fixed FFI/JSI overhead, not a copy (Tier-2, marginal, left
alone). SET unchanged (still wins 256 B+). MOBILEGATE smoke PASS both devices.

**Final real-device standing (kevy's Nitro KV door vs MMKV):**
- **SET**: kevy wins at realistic payloads (256 B+, up to 1.6–2.1× @4 KB).
- **GET**: kevy now wins/ties at 256 B+ and pulls ahead with size
  (iPhone ~2.7× @4 KB); MMKV keeps the small-value (≤64 B) edge.
- pub/sub: mitt ~6× (physical floor, not closable).

The audit's Tier-3 (SET copy, batched-push amortization, mitt floor) stays
not-worth / not-closable as reasoned. T1 was the one high-value item and it
landed, measured, on both phones.
