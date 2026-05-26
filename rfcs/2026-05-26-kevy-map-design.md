# RFC: `kevy-map` — purpose-built hashtable for kevy's keyspace

**Status:** Proposed (2026-05-26). Bound to v0.metal-4 + v0.metal-5 stones —
see `perfs/METAL-PLAN.md`. Author: admin@golia.jp + Claude (autorun).

## Why

`v0.metal-1` baseline pinned the **memory wall** as the dominant lever: single-
shard GET falls from 4.76M @ 1 key to 2.28M @ 10M keys (-52%, DRAM-bound). The
profile breakdown attributes most of that loss to two cache misses per command:

1. **Bucket-probe miss** (the hashbrown table loading the next bucket group
   from DRAM into L1)
2. **Value-pointer-chase miss** (the bucket holds `Vec<u8>` ptr/len/cap →
   another miss to read the value bytes)

`v0.metal-3` (SmallBytes SSO) just eliminated (2) for the small-string case —
delivering +12.9% on 10M-key GET. Lever #1 — the bucket-probe miss — needs
**software prefetch**: while the CPU finishes command N, issue `prefetcht0` on
the bucket that command N+1's hash points to, so its line is in L1 by the time
we need it. Stalls hidden = throughput recovered.

`std::HashMap` (hashbrown internally) does not expose bucket addresses. We have
two paths: bend std around it, or own the table. Owning the table also unlocks
**cache-conscious bucket layout** (lever #1's companion, v0.metal-5), gives us
a clean place to hang **Swiss-style SIMD group probing**, and erases any
remaining SipHash residue. So: **build `kevy-map`**, a single-trust-domain,
single-threaded-per-shard, byte-or-integer-keyed open-addressing table tuned
for this exact workload.

## Charter constraints

- **0 crates.io deps** — pure Rust + std + `kevy-hash`. No `hashbrown`.
- **`unsafe` is allowed**, scoped to `kevy-map` (sibling to `kevy-bytes`). The
  `kevy-store` crate keeps `forbid(unsafe_code)`.
- **No DoS hardening** (single trust domain) — no random seed, no rehash-on-
  collision-storm. Cheap hasher (kevy-hash::FxFmix) wired in directly.
- **Stable Rust only** (rust-version = 1.95) — no nightly `std::simd`; manual
  u64 SWAR / `core::arch` intrinsics if SIMD is wanted.
- **Correctness gate stays**: miri-clean unsafe; full kevy-store test suite +
  sharded 11/11 + clippy 0.

## Design choices (the forks, picked)

When two paths exist the higher-ceiling one wins; lower-ceiling fallbacks are
noted in case the higher one fails to measure as expected.

### 1. Probing — Swiss table (group + linear) over Robin-Hood

**Pick:** Swiss-table-style: each entry is a `(metadata_byte, full_kv_slot)`;
metadata bytes are scanned in 16-byte groups (one SIMD line or two u64s);
linear probe over groups on collision; tombstones encoded in metadata.

**Rationale:** Swiss layout is the standard for cache-friendly open addressing
(hashbrown / abseil). It separates the *metadata-byte* hot scan (16-bytes-at-
a-time, fits in 1 cache line) from the *full-key compare* (only on metadata
match → much rarer cache miss). Robin Hood gets a similar steady-state but its
displacement bookkeeping costs more on insert and gives no advantage on the
GET-heavy workload we measure.

**Lower-ceiling fallback (if Swiss is too complex first cut):** simple linear
probing with a single `Option<(K,V)>` per slot. Loses the metadata fast-scan
but takes ~30 LOC. Use only as a stepping stone to validate the wire-up; do
not ship it.

### 2. Metadata byte encoding — 7-bit `h2` + 1-bit state, abseil-style

**Pick:** Each metadata byte is one of:
- `0xFF` = empty (top bit 1, value bits 1's)
- `0x80` = deleted/tombstone (top bit 1, value bits 0)
- `0x00..=0x7F` = occupied; the 7 bits are `h2 = (hash >> 57) & 0x7F` (the top
  7 bits of the hash, mirroring abseil)

**Why this exact encoding:** the "occupied vs not" check is `byte & 0x80 == 0`
which is a single AND + compare. The h2 match check on a group of 16 is
`pcmpeqb` (SSE2) or u64 SWAR with a tiny constant — both branchless.

### 3. SIMD vs SWAR for group scan — `core::arch::x86_64::_mm_cmpeq_epi8` (SSE2),
**with a SWAR u64 fallback for portability**

**Pick:** On x86_64 (the deployment target — lx64), use SSE2 16-byte group
compare directly via `_mm_loadu_si128 + _mm_cmpeq_epi8 + _mm_movemask_epi8`.
SSE2 is baseline x86_64 so no runtime detection needed. On other arches,
compile a u64-pair SWAR fallback (handle 8 metadata bytes at a time using the
"is-zero-byte-in-word" bit trick).

**Lower-ceiling fallback:** scalar byte-by-byte scan. ~5× slower group scan
but trivially correct. Keep behind `#[cfg(...)]` for first wire-up + miri
testing, then enable the SIMD path.

**Why not nightly `std::simd`:** rust-version 1.95 locked; `core::arch` is
stable; portability story below.

### 4. Bucket-address API — first-class

**Pick:** Public method `KevyMap::probe_index_for(hash) -> usize` (returns the
table index a key with `hash` would START probing at) **plus** `KevyMap::
metadata_byte_ptr_for(hash) -> *const u8` (the address of the first metadata
byte to scan). v0.metal-5 will call the second one to issue `prefetcht0` for
the next command's bucket group while the current command finishes.

These methods are scoped under `pub fn` (no internal-only) because v0.metal-5
(software prefetch) is the *whole point* of building kevy-map; we accept the
encapsulation hole. Internal invariants stay encapsulated; only the address is
exposed.

### 5. Hasher integration — own the hash, not just the `Hasher` trait

**Pick:** `KevyMap` takes a `K: KevyHash` bound where `KevyHash::hash() -> u64`
is one direct method call (not a `Hasher::write_*` + `finish` round-trip).
`kevy-hash` exports `KevyHash for [u8]`, `for u32`, `for u64`, `for i32`
(every key type kevy-rt uses for conn maps).

**Rationale:** `std::hash::Hasher` is a state-machine API designed for compound
keys with multiple fields; for kevy's byte-string + integer keys it's overhead
(the trait dispatch + `write` + `finish` is ~3 unnecessary function frames per
hash). A direct one-call hash also lets the hasher compute both `h1` (table
index) and `h2` (metadata byte) in one pass.

### 6. Grow strategy — power-of-two with `7/8` load factor; **doubling-only**

**Pick:** Table is power-of-two sized (no modulo — index = `hash & (cap-1)`).
Load factor target 7/8 (~87.5%); when crossed, allocate a new table 2× the
size, drain-and-reinsert, free old. **Never shrink** (kevy's keyspace grows
monotonically in the common case; shrinking adds branches on the hot DEL path
and any memory benefit comes from RSS which a periodic explicit reclaim could
handle later).

**Why 7/8 (not 0.9 like hashbrown's default):** prefetch wants short probe
sequences. At 7/8 mean probes are ~1.5; at 15/16 they grow to ~3+. The extra
RSS at 7/8 (vs 15/16) is ~13%, but real-workload perf compounds with prefetch
— the higher-ceiling choice for *this* table.

### 7. Borrow + lookup ergonomics — `K: Borrow<Q>, Q: ?Sized + KevyHash + Eq`

**Pick:** `get<Q>(&self, key: &Q)` like `std::HashMap` — but `Q` must implement
`KevyHash + Eq` directly (no `Borrow` chain through `std::Hasher`). For our
case: `K = Vec<u8>`, `Q = [u8]`, and `KevyHash for [u8]` makes `map.get(b"foo"
.as_slice())` work without allocating.

### 8. Value storage — inline tuples, **not** parallel arrays

**Pick:** `slots: Box<[Slot<K,V>]>` where `Slot = MaybeUninit<(K,V)>`. The
metadata array is **separate** (so the group scan touches one cache line
without dragging k/v in), but K and V live together in one slot so the post-
match access pattern (compare key, then read value) hits one line.

**Why not SoA (slots_keys + slots_values):** for kevy's workload the key
compare + value read are sequential per slot — keeping them adjacent saves
the second-line load. The metadata/slot split is the only SoA we need.

## Surface area (minimum API)

```rust
pub struct KevyMap<K: KevyHash + Eq, V> {
    metadata: Box<[u8]>,  // capacity bytes, 0xFF = empty
    slots:    Box<[MaybeUninit<(K, V)>]>,
    len:      usize,
    // capacity-1 cached for `hash & mask`
    mask:     usize,
    // growth target = capacity * 7 / 8 (cached)
    growth_left: usize,
}

impl<K: KevyHash + Eq, V> KevyMap<K, V> {
    pub fn new() -> Self;
    pub fn with_capacity(cap: usize) -> Self;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;

    pub fn get<Q: KevyHash + Eq + ?Sized>(&self, key: &Q) -> Option<&V>
        where K: Borrow<Q>;
    pub fn get_mut<Q: ...>(&mut self, key: &Q) -> Option<&mut V> where ...;
    pub fn contains_key<Q: ...>(&self, key: &Q) -> bool where ...;
    pub fn insert(&mut self, key: K, value: V) -> Option<V>;
    pub fn remove<Q: ...>(&mut self, key: &Q) -> Option<V> where ...;

    pub fn iter(&self) -> Iter<'_, K, V>;
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V>;
    pub fn drain(&mut self) -> Drain<'_, K, V>;
    pub fn clear(&mut self);

    // v0.metal-5 hooks — exposing the bucket address so the caller can
    // prefetch the next command's group while finishing the current.
    pub fn prefetch_for_hash(&self, hash: u64);
    pub fn metadata_byte_ptr_for_hash(&self, hash: u64) -> *const u8;
}

// Entry API (defer to next iteration if not on a hot path)
```

`kevy-store::Store` calls `Borrow<[u8]>` against `K = Vec<u8>` today; this is
preserved. Hash collection variants (HashData / SetData / ZSetData.by_member)
swap their `FxHashMap` for `KevyMap` analogously.

## What's deliberately NOT in scope (for the v0.metal-4 ship)

- **Resize policies other than ×2.** Once-merged, if profiles show a different
  factor wins we revisit; v0.metal-4 ships ×2.
- **Custom allocator integration.** Use `Box::new_uninit_slice` / `Vec` for
  storage. Hooking into an arena allocator is a separate stone, post-v0.metal.
- **Entry API.** Adds ~200 LOC and a borrow dance; current store code uses
  match-on-`get_mut` instead. Defer.
- **Iterators stable across modification.** std doesn't guarantee that either;
  matches our usage.

## Validation plan

`v0.metal-4` and `v0.metal-5` ship in **one** feature branch (the prefetch is
the *whole point* of building kevy-map; merging metal-4 standalone would
publish a perf-neutral or slight-regress change). Branch:
`feature/metal-4-5-kevy-map-and-prefetch`. METAL-PLAN.md will be updated to
reflect the merge.

Correctness:
- `kevy-map` unit tests: insert/get/remove/iter/grow; collision storms (force
  long probe sequences with a degenerate hasher); tombstone handling; iter
  invariants under remove.
- `cargo miri test -p kevy-map` (the unsafe is concentrated here; miri must be
  clean).
- `cargo test --workspace` + sharded 11/11 epoll + io_uring + clippy 0.

A/B vs v0.metal-3 develop:
- `bench/metal_keyspace.sh` curve at 1/100k/1M/10M
- RSS at ~8.6M keys
- binary size on lx64
- Single-shard cache-hot `-c50 -P256` (the existing per-core ceiling number)

Judge:
- Memory-wall axis (10M-key GET): expect ≥ +10% vs metal-3 (the bucket-probe
  miss is the remaining big miss). If <+5%, profile and adjust before merging.
- RSS: expect neutral-to-slight regress (7/8 load factor vs hashbrown's 15/16
  costs ~13% bucket-array RSS, but the *value* RSS dominates).
- 1-key cache-hot: should be neutral or slight gain (one fewer SipHash call,
  one fewer trait frame; the slot layout shouldn't matter at hot-L1 sizes).

Per-charter the relaxed perf-WIN gate applies: keep what helps any axis.
But because this is the metal-4+5 *combined* stone, expect at least one axis
to clearly win — that's the whole reason we're building the table.

## Sequencing inside the combined stone

1. `kevy-map` crate with the **scalar group-scan** path (no SIMD). Unit
   tests + miri-clean. Wire into `Store::map` (other variants stay FxHashMap
   for now). Local cargo test + clippy. (~commit boundary)
2. Replace HashData / SetData / ZSetData.by_member with `KevyMap`. Local test.
3. Swap in SSE2 group scan; gate with `cfg(target_arch = "x86_64")`. Bench.
4. v0.metal-5: `prefetch_for_hash` calls woven into the command batch
   driver (`kevy-rt`); whichever loop reads commands ahead-of-time gets the
   prefetch hint for the next bucket. Bench.
5. lx64 sharded 11/11 (epoll + io_uring), A/B file, judge, merge.

Steps 1-3 give kevy-map standalone; 4 is the prefetch payoff. If step 3
already produces a clear win (because the group scan is faster than hashbrown's
on this workload — possible since hashbrown's group scan does extra work for
generic hashers), we ship without 5 *also* — but METAL-PLAN's expectation is
that 5 is the bigger lever. Either way, no premature merge.

---

This RFC is the L3a plan; deviations during execution are recorded as
amendments to this file or as separate notes under `perfs/topics/`.
