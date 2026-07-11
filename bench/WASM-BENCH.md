# WASM-BENCH — kevy-wasm vs browser storage

kevy compiled to `wasm32-unknown-unknown` (`crates/kevy-wasm` + the
hand-written `pkg/kevy.js` loader) benched against the storage options a
web app actually has: a bare `Map` (the in-realm ceiling), `localStorage`,
and raw `IndexedDB`.

- **Harness**: `crates/kevy-wasm/bench/bench.html`, driven by
  `crates/kevy-wasm/bench/run_headless.py` (Python stdlib: local HTTP
  server with COOP/COEP headers + CDP over a hand-rolled WebSocket).
  `crossOriginIsolated === true`, so `performance.now()` resolves 5 µs.
- **Environment**: HeadlessChrome 149, macOS (Apple Silicon), release
  wasm build (fat LTO). All numbers **median of 3 full runs**; run-to-run
  spread is shown as (±max−min in % of median).
- **Shapes**: 16-byte values, 7-byte keys; 20 000 point ops per axis
  (IndexedDB: 2 000, one transaction per op — its honest point-access
  shape); batch load = insert the whole dataset; scan = full keyspace
  filter (~10% match). Two dataset sizes: 1k and 100k keys.

Reproduce:

```sh
cargo build -p kevy-wasm --target wasm32-unknown-unknown --release
python3 crates/kevy-wasm/bench/run_headless.py --page bench.html --timeout 600 --out results.json
```

## Point ops (ops/s, higher is better)

| n=1 000 | point read | point write |
|---|---:|---:|
| **kevy-wasm (memory)** | **2 087 683** (±4%) | **1 726 370** (±3%) |
| bare Map | 12 158 055 (±5%) | 15 037 594 (±4%) |
| localStorage | 4 993 758 (±2%) | 376 364 (±5%) |
| IndexedDB | 24 352 (±4%) | 10 396 (±1%) |

| n=100 000 | point read | point write |
|---|---:|---:|
| **kevy-wasm (memory)** | **1 672 241** (±4%) | **1 787 310** (±4%) |
| bare Map | 10 582 011 (±8%) | 8 888 889 (±26%) |
| localStorage | 3 466 205 (±9%) | 361 696 (±10%) |
| IndexedDB | 21 692 (±4%) | 9 479 (±2%) |

kevy vs IndexedDB: **77–86× read, 166–189× write**.
kevy vs localStorage: **4.6–4.9× write**; read 0.42–0.48× (see the
root-cause section — localStorage reads are renderer-memory hits, not
storage reads).

## Batch load and full scan (ms, lower is better)

| n=100 000 | batch load | scan (~10% match) |
|---|---:|---:|
| **kevy-wasm (memory)** | **60.8** (±4%) | **5.6** (±6%) |
| bare Map | 13.2 (±1%) | 0.15 (±3%) |
| localStorage | 271.8 (±3%) | 31.9 (±5%) |
| IndexedDB | 2 218.1 (±4%) | 254.5 (±4%) |

(n=1 000: kevy 1.77 ms load / 0.39 ms scan; localStorage 2.46 / 0.32;
IndexedDB 21.3 / 3.06.) kevy loads 4.5× and scans 5.7× faster than
localStorage, and 36× / 46× faster than IndexedDB at 100k.

## Durability (persistence pump on)

Durable write = insert the whole dataset + durability barrier
(`await flush()` for kevy; transaction commit for IndexedDB).

| durable write (ops/s) | n=1 000 | n=100 000 |
|---|---:|---:|
| **kevy-wasm + OPFS** | **593 472** (±7%) | **785 268** (±2%) |
| **kevy-wasm + IndexedDB backend** | **722 022** (±2%) | **848 392** (±1%) |
| raw IndexedDB (chunked tx) | 46 915 (±7%) | 45 083 (±4%) |
| localStorage (`setItem`) | 376 364 (±5%) | 361 696 (±10%) |

kevy's pump batches write frames per microtask, so a synchronous write
burst costs one storage append: **12.6–17.4× raw IndexedDB**, and it
beats even localStorage's fire-and-forget writes ~2× while being an
actual append log.

## Restart load (cold open to usable state, ms)

| restart load | n=1 000 | n=100 000 |
|---|---:|---:|
| **kevy-wasm + OPFS** | 4.59 (±12%) | **129.2** (±1%) |
| **kevy-wasm + IndexedDB backend** | **2.14** (±9%) | **127.3** (±1%) |
| raw IndexedDB (cursor → Map) | 3.51 (±9%) | 354.0 (±2%) |
| localStorage (iterate → Map) | 0.54 (±3%) | 64.3 (±13%) |

At 100k keys kevy reopens **2.7× faster than IndexedDB** with either
backend. At 1k the OPFS backend pays a fixed ~4 ms bring-up (worker
spawn + OPFS directory/handle open — data-independent constants), where
the IndexedDB backend (same pump, no worker) already beats raw
IndexedDB; pick `persist: { backend: "idb" }` if a tiny dataset's open
latency matters more than large-dataset throughput.

## Cross-tab message RTT (ms, echo tab, 300 sequential round trips)

| payload | kevy p50 | kevy p95 | storage-event p50 | storage-event p95 |
|---|---:|---:|---:|---:|
| 64 B | 0.105 (±0%) | 0.165 (±21%) | 0.105 (±0%) | 0.140 (±4%) |
| 1 KB | 0.115 (±4%) | 0.175 (±11%) | 0.105 (±0%) | 0.140 (±4%) |
| 64 KB | **0.295** (±3%) | **0.405** (±12%) | 0.335 (±3%) | 0.425 (±12%) |

Small frames sit on the task-scheduling floor (~0.105 ms) for both
channels; kevy's BroadcastChannel bridge wins at 64 KB (structured
clone of one binary buffer vs `JSON`/UTF-16 string round trip). The
storage-event hack loses on semantics rather than latency: single-key
overwrite drops intermediate frames under a slow consumer, payloads
must be strings, every ping is written to disk quota, and the sender
tab never sees its own events.

## Verdict against the acceptance line

1. **In-memory read/write must crush localStorage/IndexedDB by an order
   of magnitude** — **met against IndexedDB** (77–189×). Against
   localStorage: writes 4.6–4.9× (not 10×), reads 0.42–0.48×. Root
   cause below; the line is not met for the localStorage column and the
   analysis says a 10× read margin is physically unreachable for any
   wasm-hosted store.
2. **Persistence at least as good as IndexedDB** — **met**: 12.6–17.4×
   durable-write throughput, 2.7× faster restart at 100k with both
   backends (the only losing cell is OPFS's fixed ~4 ms bring-up at the
   1k size, where the fallback backend already wins).
3. **Cross-tab latency below the storage-event hack** — **met at 64 KB**
   (p50 −12%, p95 −5%); tied at ≤1 KB where both channels sit on the
   same ~0.105 ms task-hop floor (p95 there is 25–35 µs worse — the cost
   of the engine enqueue + poll drain that gives kevy real pub/sub
   semantics instead of a lossy overwrite).

## Root cause: the localStorage read/write cells

Chrome's localStorage is a **renderer-memory map with an async disk
mirror**: reads never touch storage — `getItem` is a same-realm native
hash lookup returning an interned string. Its measured read rate
(3.5–5.0M ops/s) accordingly lands in the same class as a bare `Map`
(10.6–12.2M), not in the class of actual storage APIs (IndexedDB:
0.02M).

A 10× margin over localStorage reads means 35–50M ops/s — **3–4× faster
than the bare native `Map` ceiling measured in the same realm**. Every
wasm-hosted store pays, per call: one UTF-8 encode of the key, at least
one JS↔wasm boundary crossing, and one copy out of linear memory
(≈ 0.3–0.5 µs floor ⇒ ≈ 2–3M ops/s ceiling). kevy-wasm measures 1.7–2.1M
after the loader's staging optimizations (persistent scratch buffer,
`encodeInto`, cached memory view, deduped clock feed — which took the
loader from 0.77M/1.2M to 1.7M/2.1M, a 2.2× gain), i.e. it already sits
near that boundary-imposed ceiling. The comparison kevy wins is the one
that matters for a *store*: durable writes (kevy 2× localStorage,
16× IndexedDB) and everything localStorage cannot do at any speed
(binary values, TTLs, counters, scans, >5 MB datasets, pub/sub,
non-blocking persistence).

The write-side margin (4.6–4.9×, target 10× = 3.6M ops/s) is *not*
physically closed: the remaining per-write budget is dominated by the
engine's in-wasm path (shard lock + map insert + value allocation,
≈ 0.2 µs of the 0.56 µs total), shared with the native embedded hot
path. Attacks there (per-op allocation elimination in
`kevy-store::set`, registry-lookup fusion across the three
boundary calls of a `get`) belong to the engine-wide perf train, with
the native perfgate as the referee — not to a loader-side tweak. Until
that lands, the honest statement is: **order-of-magnitude over
IndexedDB on every axis; 4.6–4.9× over localStorage on writes; reads
bounded by the wasm boundary at ~0.5× of localStorage's
renderer-memory hits.**

Raw per-run JSON: 3 runs archived by the driver (`--out`); regenerate
with the command above.
