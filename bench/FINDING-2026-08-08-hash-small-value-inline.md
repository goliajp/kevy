# V4 enabler — hash small-value inline (Phase A + attack)

Status: implemented on `feature/v4-hash-small-value-inline`, local build+locgate+tests
green. **Box validation pending** (perfgate-median / memgate / crashgate+repligate).

## Why this is V4's real blocker

The industrial charter's P1 (alloc default ON) conflicts with P2 (collection
angles must not regress vs the last release) on exactly one class of workload:
the balance round priced alloc-ON as "clean everywhere except collection angles
−9~−15". The B' recovery originally planned for V4 (single-envelope pooling) was
**proven a dud** in the balance census (envelope classes = 0.5% of allocations).
The census named the true knife: HSET does ~2.9 allocations/op, of which ~2 are
**the small field values being heap-allocated**. Kill those and the collection
angle recovers — the actual enabler that lets alloc-ON not regress vs the last
release.

## Phase A — where the allocation is

The store already inlines short field *keys* and tiny whole hashes:

- field keys: `HashData = KevyMap<SmallBytes, _>` — a ≤22 B key lives inline in
  the map node, no heap.
- tiny hashes (1–2 short pairs): `Value::SmallHashInline` — entirely inline.

But once a hash promotes past the inline capacity, the value slot was
`Vec<u8>`:

```
pub type HashData = KevyMap<SmallBytes, Vec<u8>>;   // <- value heap-allocated
```

Every promoted-hash write did `map.insert(SmallBytes::from_slice(field),
value.to_vec())` — `value.to_vec()` heap-allocates for **every** field value,
even a 3-byte one. That is the ~2 allocs/op the census named (one for the value,
plus the map's own growth amortised).

Allocation sites (all in `kevy-store/src/hash.rs`): `hset_one` promote + heap
arms, `hincrby`/`hincrbyfloat`, `hset_create` heap fallback, plus the decode
paths (`tier_codec::decode_hash`, `keyspace::load_hash`, `small_hash::promote`).

## Attack — value slot `Vec<u8>` → `SmallBytes`

```
pub type HashData = KevyMap<SmallBytes, SmallBytes>;   // symmetric with the key
```

Short values (≤22 B) now live inline in the 24 B slot — **zero per-value
allocation** — exactly as the field key already does. Long values (>22 B) still
heap, and `SmallBytes::from_vec` reuses the incoming Vec's allocation on that
path (no extra copy). The slot size is unchanged (both `Vec<u8>` and
`SmallBytes` are 24 B in-node), so `HASH_SLOT_BYTES` and the map layout are
untouched.

Blast radius: 2 crates (kevy-store, kevy-persist), 7 files, ~20 mechanical read
sites (`Vec::as_slice`→`SmallBytes::as_slice`, decode `to_vec`→`from_slice`).

## Accounting decision (heap-basis, choice A)

`weight()` (full walk) and `hash_field_weight` (incremental delta) MUST share a
basis or `reweigh_entry` vs `account_delta` drift. The established convention —
already used for field keys and set members — is **charge heap bytes only**
(inline = 0), with the fixed slot constant covering the in-node bytes. So both
now charge `value.heap_bytes()`. Consequence: a hash of small values legitimately
weighs less toward maxmemory (the memory really was saved). This is a **correct
behavior shift** that memgate must re-accept on the box; it is not a bug.

The tier/compress/snapshot paths serialize the cold *byte* form and are
unaffected by the hot-representation change.

## Validation plan (box)

1. **perfgate-median** vs develop — expect HSET/HGET collection angles to
   improve (fewer mallocs/op); no regression elsewhere. This holds on the
   default glibc build too (the change removes malloc calls regardless of
   allocator), and is what lets alloc-ON clear P2.
2. **memgate** — the maxmemory accounting shift for small-value hashes; re-accept
   or rebaseline as a documented improvement.
3. **crashgate + repligate** — SmallBytes value round-trips through AOF rewrite,
   snapshot payload, and tier demote/promote.

## Box validation (lx64, f42f4bf1 vs develop-tip 0c10627e)

All measured on the box; DEV = develop-tip, V4 = DEV + this change (only diff).

**Correctness — all green:**
- workspace 2470 tests / 237 suites green (local).
- repligate PASS (SmallBytes hash value round-trips through replication).
- memgate PASS (16 B→96, 1024 B→1120, cold-key→96 bytes/entry, within ±20 %
  band — the string keyspace formulas the hash change doesn't touch).
- crashgate PASS (payload-flip open + integrity: corrupt value detected, not
  replayed — the new value format survives the AOF/snapshot crash path).

**Throughput — neutral (sub-noise):** HSET large-N median-of-5 (NH=12M, escapes
redis-benchmark's 250 ms quantization) V4 479,942 vs DEV 470,311 = +2.0 %, but
the SADD control (untouched code) swung −2.1 % → box noise band ±2-3 %. The HSET
gain is inside it. This is the §8 pattern: a per-op allocation reduction gets
pipeline-overlapped / kernel-TCP-swallowed at the throughput level.

**Memory — modest real win:** one giant hash (redis-benchmark HSET uses a fixed
key + random field → ~100k fields, 8 B values, dbsize=1), after 12M ops:
- RSS: DEV +143.5 MB vs V4 +138.8 MB → V4 saves ~4.6 MB (**~3.2 %**).
- used_memory: DEV 4.00 MB → V4 3.20 MB = **−800 KB = exactly 100k × 8 B**, i.e.
  the inline values now charge 0 heap — the accounting shift works precisely and
  memgate re-accepts it.

## P2 measurement — the alloc-ON 4-cell (the decisive run)

Two takes; the first taught a methodology lesson.

**Take 1 (P=1) — wrong shape, everything neutral.** {DEV,V4}×{ON,OFF} at
`-P 1`: all four hset cells within ±2 %. With one op per network round trip the
server is never CPU-bound, so per-op alloc cost hides under TCP latency — the
alloc-ON collection tax (balance round: −9~−15) does not even reproduce. A
neutral reading here would have been the §1 metric-mismatch trap.

**Take 2 (P=256, perfgate legacy-angle shape, server CPU-bound) — decisive:**

| cell | hset median (rps) | ON/OFF | sadd median | ON/OFF |
|---|---|---|---|---|
| DEV-OFF | 3,126,642 | — | 4,727,408 | — |
| DEV-ON | 2,946,389 | **−5.8 %** | 3,651,872 | −22.7 % |
| V4-OFF | 3,524,865 (+12.7 % vs DEV-OFF) | — | 4,613,276 | — |
| V4-ON | 3,518,664 | **−0.2 %** | 4,131,680 | −10.4 % |

- **The hash-angle alloc-ON tax is erased**: DEV pays −5.8 % (raw bands fully
  separated, every ON run below every OFF run); V4 pays −0.2 % (ON/OFF bands
  identical). The balance round's named knife — store-side hash small-value
  inlining is the alloc-ON enabler — is confirmed for the hash angle.
- **Bonus**: V4-OFF beats DEV-OFF +12.7 % median on pipelined hset — when the
  server is CPU-bound the two removed allocs/op show up as throughput even on
  glibc. (Raw bands mostly separated; call it ~+10 % with noise caution.)
- **sadd caveat**: both branches still pay an alloc-ON sadd tax; set members
  were already SmallBytes, so that tax is the kevy-alloc fast-path per-call
  cost the pacing arc priced — a different knife, out of this change's scope.
  sadd raw spread here is ±12 % (known bimodality), so its exact tax number is
  weak; the sign is not.
- Methodology: per-op alloc reductions need a CPU-bound (deep-pipeline) cell to
  be visible — P=1 cells structurally cannot price them.

**P2 status**: the hash collection angle no longer regresses under alloc-ON.
Full P2 (all 12 perfgate-median angles vs the last release) still needs the
official perfgate-median run with an alloc-ON build, and the sadd/zadd residual
tax needs its own knife (fastpath-residue RFC). Those are the remaining gates
before alloc default-ON can ship.

## Full P2 pricing — official perfgate-median ×3, alloc-ON build (post-merge)

Run on develop tip `2e242e78` (V4 hash-inline merged), `--features kevy-alloc`,
median-of-3, floor = ref × 0.92 (ref = PERF-BASELINE, the last-release default
build — the P2 ratchet semantics):

| angle | median vs ref | verdict |
|---|---|---|
| legacy_8sh_hset | **−5.5 %** | **PASS** (balance round priced this −9~−15) |
| legacy_8sh_sadd | −8.1 % | FAIL (0.1 pp below floor — edge of the noise band) |
| legacy_8sh_zadd | **−15.0 %** | **FAIL** (the real remaining distance) |
| legacy_8sh_incr / lpush / set / get | −5.7 / −3.8 / −4.3 / −2.9 % | PASS |
| pinned_cluster get/set | −1.5 / −3.0 % | PASS |
| pinned_compat get/set | −1.8 / −2.4 % | PASS |
| zalg_zinterstore | +6.0 % | PASS |

**10/12 PASS.** The hash angle is confirmed recovered on the official harness —
V4's enabler did its job. The two remaining reds are exactly the
fastpath-residue set: sadd sits 0.1 pp under the floor (edge), zadd is a real
−15 %. Per the methodology (Round 4+), zadd's distance calls for a fresh
decomposition, not another polish round; the direction choice (accept / widen
claims / class redesign — fastpath-residue RFC) is the owner's.

**Bottom line: alloc default-ON stays blocked on sadd/zadd; the hash blocker
is gone.**

Measurement-hygiene note for the record: three consecutive REFUSED runs before
this one were observer effects — (a) a zombie watcher pair from an earlier
session (mutually kept alive: each saw the other's `perfgate-median.sh`
cmdline in its `pgrep -f` loop condition) spawning `cp .../captmp/pgmed-*`
every 5 s whose argv contains "kevybench" ⊃ "kevy"; (b) my own status-polling
ssh chains (`sudo -iu kevybench …` — the gate's exclusion only matches
`sudo -u`). Fix: poll a /tmp path with a kevy-free argv and never query the
bench account while a gate is running.

## Honest disposition

Correct, all gates green, ~3 % RSS win, and — measured under the CPU-bound
cell above — **the hash-angle alloc-ON tax is erased (−5.8 % → −0.2 %) plus a
~+10-12 % pipelined-hset throughput win even on glibc**. The earlier "neutral"
readings (P=1 throughput cells) were the wrong shape, not the wrong change.
Remaining before alloc default-ON: official perfgate-median with an alloc-ON
build (full 12-angle P2), and the sadd/zadd fast-path residual (different
knife, fastpath-residue RFC). Merge decision is the owner's.

## Local status

- workspace build clean; locgate PASS (extracted `heap_hash_set` helper to keep
  `hset_one` ≤50); kevy-store + kevy-persist 213 tests green (hash ops, snapshot
  round-trip, tier codec, accounting).
- Does NOT flip the alloc default — that is a separate step gated on this
  recovery landing and on box perfgate confirming P2 is cleared.
