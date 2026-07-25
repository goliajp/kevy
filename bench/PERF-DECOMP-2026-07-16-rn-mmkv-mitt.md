# RN perf decomposition — kevy vs MMKV (KV) & mitt (pub/sub)

**Date:** 2026-07-16 · **Method:** perf-vs-foss two-phase dance, Phase A
(decomposition, read-only). **Host:** Apple M-series macOS (engine/FFI
microbench ground truth). **Device numbers** quoted from the mmkvgate /
pubsubgate LEDGERs (iOS Release iPhone 17 Pro sim + Android arm64-v8a
emulator, Hermes). **This host cannot run RN/JSI** — no device — so the JSI
hop cost is *reasoned from the measured NITROGATE numbers*, and everything
below the FFI boundary is measured here directly.

Two reference bars:
- **MMKV** — synchronous mmap KV (get = mmap-read + protobuf decode; set =
  append/override memcpy into a mapped page). Bar for RN KV get/set.
- **mitt** — a ~200-byte same-thread JS emitter (`emit` = iterate a
  handlers array, call each). Zero crossing, zero alloc. Bar for pub/sub.

---

## §0 Budget baseline — atomic op cost table (methodology §4)

Apple Silicon M-series; the numbers below are the yardstick for every µs
estimate in this doc. (Device ARM phone + Hermes are slower per-op; where a
row is device-derived it says so.)

| Op | Cost |
|---|---|
| L1 cache hit | 1 ns |
| L2 cache hit | 3–5 ns |
| L3 / DRAM | 50–100 ns |
| Atomic load (uncontended) | 1–2 ns |
| Atomic CAS (uncontended) | 5–10 ns |
| RwLock read (uncontended) | 10–50 ns |
| Mutex acquire (uncontended) | 20–50 ns |
| Heap alloc (system malloc, small) | 30–50 ns |
| BTreeMap::get (~10k) | 100–300 ns |
| HashMap::get (FxHash) | 30–50 ns |
| `format!` 6-digit int | 50–100 ns |
| `itoa` stack 6-digit | 5–10 ns |
| JSI HostFunction dispatch (Hermes, device) | ~50 ns iOS / ~110 ns Android (measured `abi()`) |
| RN CallInvoker native→JS hop (device) | ~µs-class enqueue + thread wake (see §D) |
| TCP loopback (not on this path) | — |

---

## §A Measured ground truth (this host, in-memory `kevy_open_mem`)

Throwaway harness linked kevy-ffi's real symbols (`kevy_get`/`kevy_set` vs
`kevy_cmd`), 200 warm 16 B keys, N = 2M, median of 3. **In-memory** —
matches the Nitro door, which opens `kevy_open_mem()` in its C++ ctor (so
its SET pays **no AOF** — a key point, see §C). Harness deleted after
capture; numbers frozen here.

| Path (16 B value) | ns/op (median of 3) | what it includes |
|---|---:|---|
| `kevy_set` scalar | **34.9** | shard wlock + `store.set` + `value.to_vec` |
| `kevy_get` scalar | **20.6** | shard rlock + `get_shared` + `into_owned` + KevyBuf alloc/free |
| `kevy_cmd` GET | **172.0** | argv `Vec<Vec<u8>>` copy + dispatch + RESP `$`-bulk encode + KevyBuf |
| `kevy_cmd` SET | **134.1** | argv `Vec<Vec<u8>>` copy + dispatch + `store.set` + `+OK` encode + KevyBuf |

**Derived RESP-tax at the FFI boundary (the CPU the scalar door removes):**
- GET: 172.0 − 20.6 = **151 ns/op**
- SET: 134.1 − 34.9 = **99 ns/op**

This is *native CPU only*. It does **not** include the JS-side `packAB`
(alloc Uint8Array + u32 prefixes + `TextEncoder.encode`) nor the JS-side
RESP decode a real app pays — the scalar door removes those too (§C).

Pub/sub native floor (16 B, `kevy_open_mem`, N = 1M, median of 3):

| Native path | ns/frame | what it includes |
|---|---:|---|
| publish + deliver + drain + `encode_frame` + KevyBuf + C++ vec-copy | **435.8** | the whole native side of one batched frame, pre-JSI |
| drain-only (`encode_frame` + KevyBuf + C++ vec-copy) | **260.3** | RESP `*3…message…` framing + one Vec + one std::vector copy |

So `PUBLISH` (bus `collect_delivery` + channel match + mpsc `send`) ≈ 435 −
260 = **~175 ns**, and the RESP `encode_frame` + double-copy ≈ **260 ns**.
On a phone ARM core these scale up (~1.5–2×) but stay well under the device
per-frame totals — meaning most of the *device* per-frame cost is JSI/JS,
not the engine (§D).

---

## §B Device ground truth (from LEDGERs, for the crossing cost)

**iOS Release** (iPhone 17 Pro sim, arm64, embedded bundle), median of 2:

| Axis | ops/s | ns/op | note |
|---|---:|---:|---|
| `abi()` pure JSI | ~20,000,000 | **~50** | the JSI HostFunction floor |
| `cmd` PING | 1,886,792 | **530** | 1 crossing, trivial verb |
| `cmd` SET | 1,639,344 | **610** | argv pre-packed (hoisted), reply discarded |
| pub/sub poll | 1,063,851 | 940 /frame | publish + 2× `subNext` |
| pub/sub push/msg | 877,193 | 1140 /frame | 1 CallInvoker hop / frame (< poll) |
| pub/sub batched | 1,388,917 | **720 /frame** | ~7–8 frames / hop |

**Android Debug** (arm64-v8a emulator, Hermes), median of 2:

| Axis | ops/s | ns/op | note |
|---|---:|---:|---|
| `abi()` pure JSI | 9,090,909 | ~110 | JSI floor |
| `cmd` PING | 487,805 | 2050 | |
| `cmd` SET | 295,858 | 3380 | |
| pub/sub poll | ~520,000 | ~1900 /frame | |
| pub/sub push/msg | ~235,000 | ~4250 /frame | 1 hop/frame — the hop is expensive |
| pub/sub batched | ~675,000 | **~1480 /frame** | ~7–8 frames / hop |
| **mitt** | 3,850,000 | **260 /frame** | in-process floor |

> ⚠ The `nitroBench` cmd loops **hoist `packAB` out of the loop** and
> discard the reply. So the measured `cmd SET` 610 ns (iOS) is *crossing +
> C++ unpack + `kevy_cmd` + reply-ArrayBuffer alloc/return* — it already
> **excludes** packAB and JS decode. A real app pays those on top, which
> makes the scalar door's real-world win *larger* than the loop suggests.

---

## §C Decomposition 1 — RN KV get/set: kevy vs MMKV

### MMKV reference path (file:line from the checkout)

- **get** — `Core/MMKV_IO.cpp:646` `getDataForKey` → `:624`
  `getRawDataForKey`: `m_dic->find(key)` (a hashmap lookup) → `:641`
  `itr->second.toMMBuffer(basePtr)` — a **zero-copy MMBuffer view** over the
  mmap'd page at `basePtr+offset`, then a protobuf varint decode of the
  length prefix. The value is then copied out to the caller's `Data`. Atomic
  ops: 1 hashmap get (~30–50 ns) + varint decode (~5 ns) + 1 value copy
  (memcpy, size-scaled). **Est ~150–250 ns for 16 B** on device (dominated
  by the ObjC `Data` bridge + lock). LEDGER-measured (sim): **~425 ns**.
- **set** — `Core/MMKV_IO.cpp:662` `setDataForKey` → (200 warm keys, so not
  the single-key override path) `:1057` `appendDataWithKey` →
  `doAppendDataWithKey`: `ensureMemorySize` then **one memcpy of
  (varint keylen, key, varint vallen, value) into the mmap'd page** at the
  append offset. No syscall (the page is mapped; the OS writes back lazily).
  Atomic ops: 1 hashmap get + 1 in-page memcpy (size-scaled) + offset bump.
  Periodic `fullWriteback` (`:1140`) compacts. LEDGER-measured (sim):
  **~565 ns**; on real ext4 (lx64) MMKV set-4KB is 11.84 µs vs kevy 2.98 µs.

### kevy — path (a): the CURRENT RN path (Nitro `cmd(ArrayBuffer)`)

A GET through the door as it exists today. Stage-by-stage, iOS device
numbers reasoned from §B (`cmd SET` 610 ns / `cmd PING` 530 ns / `abi`
50 ns) plus this-host FFI splits from §A.

| # | Stage | file:line | atomic ops | µs est (iOS) |
|---|---|---|---|---:|
| a1 | JS `packAB(["GET",k])` **(hoisted in bench; REAL app pays it)** | `nitroBench.ts:23` | alloc Uint8Array + DataView + `TextEncoder.encode`×2 + setUint32×2 + `set` copy | ~0.1–0.2 (est, JS) |
| a2 | JSI crossing (ArrayBuffer by ref) | Nitrogen glue | HostFunction dispatch | ~0.05 (measured `abi`) |
| a3 | C++ argv unpack | `HybridKevyNitro.cpp:25-42` | parse u32 prefixes → **2 std::vector** (`ptrs`,`lens`) allocs | ~0.05–0.10 (est) |
| a4 | `kevy_cmd`: argv `Vec<Vec<u8>>` copy | `kevy-ffi/src/lib.rs:152-156` | argc heap allocs + memcpy each arg | part of a5 |
| a5 | dispatch + `store.get` + RESP `$`-bulk encode | `ops.rs:360`, `ops.rs:67` | rlock + `get_shared` + `into_owned` + `format!` `$len\r\n` | **0.172 measured (host FFI, GET)** |
| a6 | `takeBuf`: reply → std::vector copy + `ArrayBuffer::move` | `HybridKevyNitro.cpp:14-18` | 1 std::vector alloc+copy + 1 JSI ArrayBuffer object | ~0.05–0.10 (est) |
| a7 | JSI return ArrayBuffer | Nitrogen glue | HostFunction return | ~0.05 |
| a8 | JS RESP decode (extract value from `$…`) **(REAL app pays)** | app code | Uint8Array wrap + `TextDecoder`/slice | ~0.1–0.2 (est, JS) |

**Loop-measured (iOS, a2–a7 only, packAB+decode hoisted): `cmd SET`
610 ns, `cmd PING` 530 ns.** A GET returns a real bulk reply (a6 copies the
value out), so `cmd GET` ≳ `cmd SET`. Real-app GET including a1+a8 ≈
**0.8–1.0 µs** on iOS, **~3–4 µs** on Android.

### kevy — path (b): PROPOSED Nitro scalar door (`getData`/`setData`)

Add `getData(key): ArrayBuffer | undefined` and
`setData(key: ArrayBuffer, value: ArrayBuffer): void` to the spec, calling
`kevy_get`/`kevy_set` directly. Stages **removed** vs path (a):

| Removed stage | Why gone | µs saved (iOS est) |
|---|---|---:|
| a1 packAB | key/value passed as ArrayBuffers directly, no u32-prefix packing, no verb encode | ~0.1–0.2 (real app) |
| a3 C++ argv unpack | no packed argv to parse | ~0.05–0.10 |
| a4 argv `Vec<Vec<u8>>` copy | `kevy_get` takes `key: *const u8, len` — no argv | (in a5 delta) |
| a5 RESP encode | scalar returns raw value bytes; `setData` returns **void** (no reply at all) | GET 0.151 / SET 0.099 (host FFI delta, §A) |
| a6 reply framing | `setData` void → **no reply ArrayBuffer**; `getData` returns the value ArrayBuffer directly (still 1 copy, unavoidable) | ~0.05 (SET: whole a6 gone) |
| a8 JS RESP decode | value returned raw — no `$…` to parse | ~0.1–0.2 (real app) |

Remaining (irreducible): **one JSI hop** (a2/a7, ~50 ns iOS / ~110 ns
Android) + `kevy_get`/`kevy_set` (measured 20.6 / 34.9 ns host in-mem) +
**one value copy** (`takeBuf`, engine owns the Vec, JS owns the
ArrayBuffer — the C++ comment at `HybridKevyNitro.cpp:11-13` calls this out
as unavoidable).

**Estimated scalar-door per-op (iOS):**
- `getData` ≈ hop 50 + `kevy_get` ~30 + 1 value copy ~40 ≈ **~120–200 ns**
  (vs current real-app GET ~0.8–1.0 µs → **~4–6× faster**, and vs MMKV
  ~425 ns → **kevy ~2–3× faster**).
- `setData` ≈ hop 50 + `kevy_set` ~35 (in-mem) ≈ **~100–150 ns**
  (vs current `cmd SET` 610 ns → **~4–6× faster**; vs MMKV set ~565 ns →
  **kevy faster — IF in-memory**).

### Verdict — decomposition 1

- **GET:** kevy already **beats MMKV on the scalar lane** (mmkvgate:
  1.5–3.2× faster, KevyKit direct). The Nitro `cmd` door's RESP tax
  (measured 151 ns FFI CPU + JS packAB/decode) is the *only* thing making
  RN GET look slow. **The scalar door removes it → RN kevy GET beats MMKV.**
  High confidence.
- **SET:** two regimes. (i) **In-memory** (the Nitro door's current
  `kevy_open_mem`): scalar `setData` ~100–150 ns crushes MMKV — but it is
  **not durable**. (ii) **Durable** (if the door opened `kevy_open` with
  AOF): SET is gated by the **AOF-vs-mmap architectural axis**, already
  decomposed in `mmkvgate/LEDGER.md` + `PERF-FINDING-2026-07-15-mmap-aof-
  refuted.md` — on real ext4 kevy beats MMKV 3.97×; on the iOS sim it
  loses; **device TBD**. The crossing/RESP tax is NOT the SET bottleneck;
  the durability model is. The scalar door fixes the crossing regardless;
  the durable-SET question is orthogonal and already has its own ground
  truth.
- **Irreducible floor:** one JSI hop (~50 ns iOS / ~110 ns Android) + one
  value copy (`takeBuf`). Everything else in path (a) is removable tax.

---

## §D Decomposition 2 — RN pub/sub batched-push: kevy vs mitt

### mitt reference path (file:line)

- `node_modules/mitt/dist/mitt.mjs:1` `emit`: `n.get(t)` (Map lookup) →
  `i.slice().map(fn => fn(e))` — iterate the handlers array, synchronous
  call each. **Zero crossing, zero native alloc** (the `slice()` is a small
  JS array copy). Atomic ops: 1 Map get + N closure calls. Device-measured:
  **260 ns/frame** (Android Hermes). This is a same-thread function call —
  physically unbeatable by anything that crosses a thread boundary.

### kevy batched-push path (`subscribePushBatched`)

Native poller parks in `kevy_sub_wait`, wakes, drains all pending, delivers
**one array-of-ArrayBuffers per batch** in one CallInvoker hop.

| # | Stage | file:line | atomic ops | ns/frame |
|---|---|---|---|---:|
| p1 | poller wakes (`kevy_sub_wait`) then drains rest (`kevy_sub_next`) | `HybridKevyNitro.cpp:148,159` | mpsc `recv`/`try_recv` | ~part of p2 |
| p2 | engine `encode_frame` (RESP `*3…message…`) + KevyBuf alloc | `kevy-ffi/src/lib.rs:414-429` | 3–4 `format!` bulk headers + 1 Vec | **260 measured (host, drain-only)** |
| p3 | per-frame std::vector copy (KevyBuf → vector) | `HybridKevyNitro.cpp:153,160-162` | 1 std::vector alloc + memcpy | ~part of p2's 260 |
| p4 | **per-frame `ArrayBuffer::move`** (1 JSI ArrayBuffer object + shared_ptr) | `HybridKevyNitro.cpp:155,162` | 1 JSI alloc + control block | ~200–400 (Hermes est) |
| p5 | `batch.push_back` into `std::vector<shared_ptr>` | `HybridKevyNitro.cpp:155,162` | amortized vector growth | ~5 |
| p6 | **1 CallInvoker hop per batch** (amortized /~8) | Nitro AsyncJSCallback | enqueue on JS loop + thread wake | ~375 (see below) |
| p7 | JS: iterate `frames[]`, call handler per ArrayBuffer | `nitroBench.ts:166` | array iterate + closure×M | ~100–200 (est) |

**Hop cost derivation:** push/msg (1 hop/frame) = ~4250 ns/frame Android;
poll (no hop, 2 crossings) = ~1900 ns/frame. The single CallInvoker hop is
the dominant term of push/msg — a full enqueue-onto-JS-loop + thread-wake,
**~3 µs**, not a bare JSI call. Batched amortizes it across ~7–8 frames →
~375–430 ns/frame. That is why **push/msg LOSES (0.4–0.5×) but batched WINS
(1.3×)** — the LEDGER's load-bearing result.

Sum (Android): p2/p3 ~260 (host; ~400–600 device ARM) + p4 ~200–400 +
p6 ~375 + p7 ~150 ≈ **~1.4–1.5 µs/frame** — matches measured ~1480 ns.

### The closable gap

**Attack — pack the whole batch into ONE length-prefixed ArrayBuffer.**
Instead of M `ArrayBuffer::move` (p4, M JSI objects) + a
`std::vector<shared_ptr<ArrayBuffer>>`, the poller memcpys each frame
(u32-LE length prefix + bytes) into **one** growing buffer, delivers **one**
ArrayBuffer + a count; JS slices `Uint8Array` **views** (no copy) per frame.

- p4 collapses from **M JSI allocs → 1** per batch. Saved ≈ (M−1)/M ×
  ~200–400 ns ≈ **~200–350 ns/frame** (Hermes est).
- p3 collapses from M separate std::vectors → M memcpys into one buffer
  (fewer allocations, same total bytes) — saves the per-frame alloc header,
  ~30–50 ns/frame.
- p7 JS side: `subarray` views instead of receiving M ArrayBuffers — a
  view is a pointer+len, cheaper than an ArrayBuffer object hand-off.

**Estimated batched after packing (Android):** ~1480 − ~250 (p4) − ~40 (p3)
≈ **~1.15–1.2 µs/frame** → mitt gap **5.5× → ~4.4×**. iOS proportionally:
720 → ~550 ns/frame → gap narrows similarly.

**Attack — scalar (RESP-free) frame drain (`kevy_sub_next_raw`).** p2's
260 ns/frame (measured) is RESP `encode_frame` — 3–4 `format!` bulk headers
per frame. For a known-channel push subscriber the RESP `*3…message…`
wrapper is pure tax: the JS side only wants the payload. A
`kevy_sub_next_raw` returning **just the payload bytes** (the pub/sub analog
of the KV scalar door) removes the `encode_frame` allocs. Saves most of the
260 ns/frame (host) / ~400–600 ns (device ARM). Combined with the packing
attack, batched → the raw-payload-into-one-buffer shape: **memcpy payload +
u32 prefix into one buffer, 1 ArrayBuffer, 1 hop**. Estimated batched →
**~700–900 ns/frame Android** → mitt gap **~2.7–3.5×**.

### Verdict — decomposition 2

- **mitt's 260 ns/frame zero-crossing floor is unbeatable** for raw
  throughput — it never leaves JS, never allocates natively. Per §1 this is
  a *physical* floor, not a hand-wave: any native bus pays (a) a native
  payload alloc/encode per frame and (b) an amortized CallInvoker hop per
  batch. Naming it is honest; it is not a reason to stop.
- **The realistic target is the single-JSI-hop-per-batch + one-buffer-alloc
  floor.** Two stacked attacks — batch into one ArrayBuffer (kills M−1 JSI
  allocs) and raw-payload drain (kills the RESP encode) — take batched from
  ~1.48 µs → est ~0.7–0.9 µs/frame on Android, closing mitt from 5.5× to
  **~3×**. That is a materially different product, and every step is a
  concrete code change, not a ceiling claim.
- **push/msg stays a pessimization** (1 hop/frame ≈ 3 µs) — do not chase it;
  batched is the shape.

---

## §E Top-N actionable attacks (sorted by µs gain)

Gains are **per-op / per-frame**; "measured" = this-host FFI split,
"est" = reasoned from device NITROGATE + cost table. **All device-facing
gains need a real-device bench to confirm** (this host has no JSI).

| # | File:line | Concrete code change | Gain | Semantic class | Blast radius |
|---|---|---|---:|---|---|
| 1 | `bindings/nitro/src/specs/KevyNitro.nitro.ts:19` + `HybridKevyNitro.cpp/.hpp` | Add scalar door: `getData(key: ArrayBuffer): ArrayBuffer\|undefined` → `kevy_get`; `setData(key, value: ArrayBuffer): void` → `kevy_set`. Removes packAB, argv unpack, RESP encode, reply framing, JS RESP decode. | GET **~4–6×** (est ~120–200 ns vs ~0.8–1 µs real-app; **beats MMKV ~425 ns**); SET **~4–6×** crossing (in-mem). FFI CPU removed: **151 ns GET / 99 ns SET (measured)**. | Additive API, no engine change | `bindings/nitro` only (~40 LOC C++ + 2 spec lines). **Needs device bench.** |
| 2 | `bindings/nitro/cpp/HybridKevyNitro.cpp:152-164` + spec `:38-41` + `nitroBench.ts:166` | `subscribePushBatched`: pack drained frames into **one** length-prefixed ArrayBuffer (u32-LE len + bytes per frame); JS slices `Uint8Array` views. M `ArrayBuffer::move` → 1/batch. | **~200–350 ns/frame** (est, kills M−1 JSI allocs); batched mitt gap 5.5× → ~4.4× | API change: `onBatch(frames: ArrayBuffer[])` → `onBatch(buf: ArrayBuffer, count)` | `bindings/nitro` cpp + spec + example. **Needs device bench.** |
| 3 | `kevy-ffi/src/lib.rs:312` (new `kevy_sub_next_raw`) + `kevy-embedded` pubsub payload accessor | Scalar pub/sub drain returning **just the payload** (no RESP `*3…message…` wrapper) for known-channel push subscribers — the pub/sub analog of #1. Kills `encode_frame`'s 3–4 `format!` per frame. | **~260 ns/frame (measured host `encode_frame`)** / ~400–600 ns device; stacks with #2 → batched → ~0.7–0.9 µs/frame, mitt gap → ~3× | Additive FFI symbol; engine exposes raw payload | `kevy-ffi` + `kevy-embedded` pubsub (~30 LOC). Binding RESP parser unaffected (new lane). **Needs device bench.** |
| 4 | `kevy-ffi/src/lib.rs:152-156` | (minor, subsumed by #1 for the scalar lane) In `kevy_cmd`, the argv `Vec<Vec<u8>>` copy is 1 heap alloc + memcpy per arg. For the RESP lane that stays, a smallvec/stack argv for argc≤N would trim it. | ~part of the 99–151 ns RESP tax; only matters for the RESP `cmd` lane callers who don't move to #1 | Internal, no API change | `kevy-ffi` only. Low priority once #1 exists. |

### What is measured vs estimated vs device-only

- **Measured (this host, frozen in §A):** scalar vs RESP FFI split
  (34.9 / 20.6 / 172.0 / 134.1 ns), native pub/sub per-frame
  (435.8 / 260.3 ns). These ground attacks #1 (the 151/99 ns RESP tax is
  real CPU) and #3 (the 260 ns `encode_frame` is real).
- **Estimated (device-reasoned):** the JSI hop (~50 ns iOS / ~110 ns
  Android, from measured `abi()`), the CallInvoker hop (~3 µs, derived from
  push/msg − poll), per-frame `ArrayBuffer::move` (~200–400 ns Hermes), JS
  packAB/decode (~0.1–0.2 µs). All labeled "est" in-line.
- **Needs a real device to verify:** every per-op *product* number for
  attacks #1–#3 — this host cannot execute JSI, Hermes GC, or the
  CallInvoker. The FFI-level gains are certain; how they translate through
  the crossing is device-gated. **Recommended Phase B gate:** build #1 into
  the Nitro door, run `nitroBench` on iOS + Android, confirm `getData` beats
  MMKV and `setData` beats `cmd SET` before declaring.

### Honesty ledger (§1 compliance)

- No "architectural ceiling" claim: the SET durable-vs-MMKV question is
  named as the AOF-vs-mmap axis with its own decomposition + real-hardware
  ground truth (kevy 3.97× on ext4), not waved off.
- mitt's floor is called *physical* (zero-crossing same-thread call) and
  quantified (260 ns), and a concrete lower target (~3× via #2+#3) is
  given — not "workload not amenable."
- Every gain is a file:line + concrete change; device products are labeled
  estimate and gated on a real-device bench, not asserted.
