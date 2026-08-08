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

## Honest disposition

Correct, throughput-neutral, ~3 % RSS win, all gates green — a keepable memory
efficiency improvement at zero throughput cost, but NOT the dramatic
collection-tax recovery the hypothesis reached for. **V4's actual purpose (does
this let alloc-ON clear P2 — not regress collections vs the last release)
remains unmeasured**: that needs an alloc-ON build A/B (kevy-alloc feature on,
V4 vs last-release-equivalent). The glibc throughput being neutral suggests the
alloc-ON recovery, if any, is also modest — but it is the logical next
measurement before claiming V4 is unblocked. Merge decision + the alloc-ON
follow-up are open for the owner.

## Local status

- workspace build clean; locgate PASS (extracted `heap_hash_set` helper to keep
  `hset_one` ≤50); kevy-store + kevy-persist 213 tests green (hash ops, snapshot
  round-trip, tier codec, accounting).
- Does NOT flip the alloc default — that is a separate step gated on this
  recovery landing and on box perfgate confirming P2 is cleared.
