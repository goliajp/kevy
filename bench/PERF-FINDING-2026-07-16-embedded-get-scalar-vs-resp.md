# Finding — embedded high-level GET: scalar shared lane vs RESP cmd path

**Date:** 2026-07-16 · **Context:** v4 client quality arc (all 7 language clients).

## Measurement (Go host microbench, mem:// backend, median of many iters)

`DB.GetScalar` (kevy_get_shared, zero-copy Arc lane) vs `DB.CmdBytes("GET", key)`
(full RESP encode → kevy_cmd → RESP bulk reply → client parse):

| value | scalar ns/op | resp ns/op | speedup | scalar allocs | resp allocs |
|-------|-------------:|-----------:|--------:|--------------:|------------:|
| 16 B  |  359 |  935 | **2.6×** | 2 | 4 |
| 4 KB  |  604 | 1428 | **2.4×** | 2 | 4 |
| 64 KB | 3551 | 6988 | **2.0×** | 2 | 4 |

RESP framing (serialize the GET request + serialize the bulk reply + parse both
directions) costs ~575 ns even at 16 B — more than the entire scalar call. This
is a MATERIAL structural win (framing avoidance), not sub-noise polish. The
Pre-Phase-B gate PASSES: routing high-level embedded GET to the scalar lane is
justified.

## The WRONGTYPE catch (cross-cutting correctness)

The scalar lane (`kevy_get_shared`) returns `1` hit / `0` miss / `-2` on any
store error — and `get_shared_owned`'s only error is `WrongType`. So a GET on a
non-string key collapses to `-2`, which a naive client reports as opaque
"misuse" (or, worse, as a miss). The RESP path preserves WRONGTYPE as a typed
error. Any client that reroutes high-level GET to the scalar lane MUST restore
that fidelity — the orthodox fix is: fast scalar path for the common
correct-type case, fall back to the framed GET on the scalar lane's error so the
typed WRONGTYPE surfaces (matching the remote backend). SET has no WRONGTYPE
(it overwrites any type), so SET reroutes need no fallback.

## Policy (End-state A — uniform, applied across all clients)

High-level/typed embedded GET → scalar shared lane + WRONGTYPE fallback;
embedded SET → scalar lane. Per-client status after the arc:

- **Rust** — direct `Store` API (no FFI framing); WRONGTYPE is a `StoreError`. Optimal + correct. ✅
- **Go** — scalar + framed fallback. Fixed (was a fresh regression: reroute without fallback). ✅ + test.
- **TS** — scalar + framed fallback (Bun). ✅
- **Java** — scalar (fastGet) but collapsed WRONGTYPE → `Optional.empty()`. Correctness fix pending.
- **Python** — high-level was RESP-only; reroute to scalar + fallback pending.
- **C#** — high-level was RESP-only (KevyDb scalar is string-keyed); reroute + binary-safe scalar pending.
- **C++** — embedded `EmbeddedStore::get` already scalar; WRONGTYPE surfaces as "misuse" — fix pending.
