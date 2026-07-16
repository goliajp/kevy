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
- **Java** — scalar (fastGet); WRONGTYPE now signals across the JNI boundary (`-2` throws) → framed fallback surfaces `Store(WrongType)`. Fixed. ✅ + test.
- **Python** — high-level embedded GET/SET routed to the scalar lane + framed fallback on GET. Fixed. ✅ + test.
- **C#** — added a binary-safe `byte[]`/`ReadOnlySpan<byte>` scalar path to `KevyDb`; unified GET/SET route to it + framed fallback on GET. Fixed. ✅ + test.
- **C++** — embedded `EmbeddedStore::get`/`get_view` fall back to the framed GET on the scalar error → typed `Store(WrongType)`. Fixed. ✅ + test.

All seven clients now uniformly: fast scalar lane for embedded GET/SET, WRONGTYPE
preserved via framed fallback (or, for Rust, direct `StoreError`). Each fix is
locked by a GET-on-non-string test on both the embedded and remote backends.

---

## Addendum — contended-read-lock bench REFUTES the read-lock-split hypothesis (#27)

The kevy-embedded audit hypothesized that sibling reads (hget/exists/smembers/…)
and GET-under-eviction serialize on the shard WRITE lock, and that a read-lock
lane would let readers scale across cores (P0-1 / P0-3b). Pre-Phase-B gate bench
(N threads, back-to-back GET, 10k keys, M-series host), comparing the two lanes
that already exist — mm=0 (rshard/read lock) vs mm=8G+NoEviction (wshard/write
lock):

| threads | rshard Mops | wshard Mops | ratio |
|--------:|------------:|------------:|------:|
| 1 | 25.4 | 32.8 | 0.77× |
| 2 | 17.8 | 20.6 | 0.86× |
| 4 |  9.2 | 14.2 | 0.65× |
| 8 |  4.6 |  5.6 | 0.82× |

**Refutation (two facts):** (1) NEITHER lane scales across cores — both degrade
~5-6× from 1→8 threads. High-frequency point reads contend on the per-shard
`RwLock` word's cache line; a read lock's atomic reader-count RMW ping-pongs the
same line as a write lock, so the read lock does NOT deliver read scaling under
this contention profile. (2) rshard is not faster than wshard — slightly slower,
because the mm=0 GET path (`get_shared` Cow + `into_owned`) does at least as much
work as the mm>0 `live_entry` path; the lock is not the bottleneck.

**Consequences:**
- **P0-1 (sibling read-lock split) + P0-3b (atomic-clock LRU): NOT pursued** —
  the throughput-scaling premise is refuted at max contention (and it's a large
  keyspace-layer / Entry-layout change). Real read scaling would need finer lock
  sharding or a lock-free read path (seqlock/RCU) — a separate, much larger
  effort, only if a realistic-workload bench later shows a material gap.
- **Kept, on non-throughput grounds:** P0-2 (counters → read lock) is a LATENCY
  fix (a big DBSIZE/INFO aggregation must not hold write locks blocking all
  writes); P1-1 (verb `to_ascii_uppercase` alloc) is an independent per-op alloc
  removal; P0-3a (NoEviction GET → read lock) is lock-CORRECTNESS (a non-mutating
  GET shouldn't exclusively lock out a concurrent writer on its shard) — NOT a
  throughput win, framed honestly.
- **Honesty correction:** GET's rustdoc claim that "concurrent readers scale
  across cores" OVER-claims — reads contend on the per-shard lock word; scaling
  is bounded by shard count, not lock-free. The doc note must say so.
