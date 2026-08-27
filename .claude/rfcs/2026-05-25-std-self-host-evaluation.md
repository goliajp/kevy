# RFC: std self-host evaluation — what kevy should own below `std`

**Status:** Living — **Tier 1 DONE** (`kevy-hash`, keyspace, v0.perf-2) + **Tier 2.1 DONE** (value-type maps, v0.perf-6). Tier 2.2 (integer conn maps) + Tier 3 open, benchmark-gated. Read-path redundancy also fixed (topics 02/03: GET ~28 ns, SET ~70 ns, INCR ~80 ns).

**Author:** admin@golia.jp + Claude (autorun, 2026-05-25)

**Method:** modelled on `mailrs/.claude/rfcs/20260522-dep-self-host-evaluation.md`. Adapted for kevy's hard constraint: **zero crates.io dependencies**, so the "deps" under scrutiny are not third-party crates — they are the **`std` components** kevy leans on. The question is identical in spirit: *which general-purpose `std` building block quietly caps our ceiling, where domain-specific knowledge would beat it?*

## Framing

kevy's north star is the hardware ceiling; beating valkey 9.1 on `-c1` / `-c50` / pub-sub is the floor. The dev host is permanently loaded by other projects, so clean full-system throughput is unmeasurable right now. The escape (this RFC's whole basis) is **measure-first at the component level**: per-crate micro-benchmarks via the self-hosted `kevy-bench` harness, run back-to-back so the **ratio survives a loaded host** even when absolute ns drift.

**This RFC does not propose self-hosting everything.** Most `std` types kevy uses (`Vec`, `VecDeque`, `BTreeSet`, the hashbrown table *inside* `HashMap`) are excellent and unbeatable by us. The evaluation surfaces only the places where kevy-specific constraints (single-trust-domain, single-threaded-per-shard, byte-string or integer keys) beat the general-purpose default.

## Evaluation criteria (per component)

- **Hot-path delta** — how often is it on the per-command / per-connection hot path?
- **Upstream ceiling** — does `std` already squeeze the work, or is there room?
- **Self-host LOC** — how big is the replacement?
- **Risk** — correctness invariants, especially anything `unsafe`.
- **Strategic value** — does owning it move a headline number?

## The audit (every `std` collection kevy touches)

From `grep std::collections` across the workspace (2026-05-25):

| Component | Where | Hot path? | Verdict |
|---|---|---|---|
| `HashMap<Vec<u8>, Entry>` keyspace | `kevy-store` | **Yes — every GET/SET/DEL** | **Tier 1** — hasher only (table stays) |
| `HashData`/`SetData` (`HashMap`/`HashSet<Vec<u8>>`) | `kevy-store` | Yes — per hash/set cmd | **Tier 2** — same hasher swap |
| `HashMap<u64,Conn>` / `<i32,u64>` / `<u64,UringConn>` | `kevy-rt` | Yes — per event/conn | **Tier 2** — integer-keyed, see below |
| `HashMap<usize, _>` `by_shard` scatter | `kevy-rt` exec | Per multi-key cmd | **Tier 3** — `Vec`-indexed by shard |
| `HashSet<&Vec<u8>>` set-ops | `kevy-rt` reduce | Per SINTER/SUNION/SDIFF | Tier 3 — revisit after Tier 1 hasher exists |
| `VecDeque` (list type, pending ring, backlog) | store / rt | Yes | **Keep** — `VecDeque` is the right ring |
| `BTreeSet<(Score,Vec<u8>)>` zset by-score | `kevy-store` | Per zset cmd | **Keep** — ordered structure, std B-tree is good |
| `Vec<u8>` (string value, buffers) | everywhere | Yes | **Keep** — unbeatable |

---

## Tier 1 — keyspace hasher (MEASURED, green-lit)

**Replaces:** the *default hasher* of the keyspace `HashMap`, **not** the map. std's `HashMap` is already a hashbrown Swiss table — world-class, we will not rebuild it. Its default hasher is `SipHash-1-3` (random-seeded, DoS-resistant), which a single-trust-domain, single-threaded-per-shard keyspace does not need.

**Why kevy-specific beats `std`:** no adversary can pick colliding keys across a trust boundary inside one shard, so we can drop SipHash's collision-hardening for a faster hash — *provided* it still avalanches well enough to not cluster in the table.

### Evidence (`crates/kevy-store/examples/bench_keyspace.rs`, 3 runs, loaded host ~load 8, ratios vs SipHash)

Two naive candidates were **measured and rejected** — this is the whole point of measure-first:

| Candidate | hash_one | get_hit | get_miss | Verdict |
|---|---|---|---|---|
| **FNV-1a** (byte-at-a-time) | 0.4–0.8× | 0.45–1.2× | 0.4–0.95× | ❌ slower — byte-at-a-time loses to SipHash-1-3's word-at-a-time |
| **FxHash raw** (no finalizer) | **4–8×** | short **0.02–0.03×** (clusters!) | 0.6–4.75× erratic | ❌ fast hash, catastrophic clustering on low-entropy sequential keys |
| **Fx + fmix64** (this RFC) | **4×** | **1.2–2.8×** stable | **1.1–1.7×** stable | ✅ fast *and* well-distributed |

The winner: **FxHash-style word-at-a-time write + murmur3 `fmix64` finalizer.** The finalizer (~6 ALU ops, once per hash) is *essential* — raw Fx clusters 30–50× on keys like `"key:0".."key:99999"`. Constants: seed `0x517cc1b727220a95`, rotate 5; finalizer `fmix64`.

**Decision:** **DONE (v0.perf-2).** Shipped as the `kevy-hash` crate (`FxHasher` + `fmix64`, `FxBuildHasher`/`FxHashMap`/`FxHashSet`), wired into `kevy_store::Store.map`. Correctness: kevy-store 22 tests green on the new hasher; kevy-hash ships a clustering-guard test. Production hot path now (Fx+fmix64): Store::get_hit ~124 ns, get_miss ~44 ns, set ~228 ns. See `perfs/topics/01-keyspace-hasher.md`.

**Risk:** low. Pure-safe Rust (`from_le_bytes` + `try_into`, no `unsafe`). The only correctness concern is hash quality → covered by the get_hit/get_miss distribution measurement above, and by the existing `kevy-store` test suite running on the new hasher.

---

## Tier 2 — same hasher for value-type maps + an integer hasher

Once `kevy-hash` exists, two follow-ups, **each benchmark-gated** (only ship if the micro-bench shows ≥ the Tier-1-class win on its own workload):

1. **`HashData` / `SetData` / zset `by_member`** (hash/set/zset value types) — **DONE (v0.perf-6).** Same byte-string keys as the keyspace → the Fx+fmix64 win transfers directly (identical `HashMap<Vec<u8>,_>`/`HashSet<Vec<u8>>` shape, already measured in topic-01). Switched the aliases to `kevy_hash::FxHashMap`/`FxHashSet` + construction `::new()`→`::default()`. kevy-store + kevy-persist (snapshot/AOF) + kevy sharded all green.
2. **Integer-keyed maps** (`conns: HashMap<u64,Conn>`, `fd_to_conn: HashMap<i32,u64>`, uring `HashMap<u64,UringConn>`) — **still open.** Single `u64`/`i32` key; SipHash on 8 bytes is waste. `FxHasher` already specializes `write_u64` to one mix, but a dense `Vec<Option<_>>` slot map (conn ids monotonic) may beat even that. **Measure first** (own kevy-rt bench): `FxHashMap` vs `Vec`-slot on connect/lookup/disconnect. In kevy-rt (unsafe/io_uring) → a larger, separate unit.

## Tier 3 — structural, lower priority

- **`by_shard: HashMap<usize, _>`** in the scatter/gather (`exec.rs`) — `usize` keys in `0..nshards` (small, dense). A `Vec<_>` indexed by shard removes the hash entirely. ~Trivial; do opportunistically.
- **`HashSet<&Vec<u8>>`** in SINTER/SUNION/SDIFF — revisit once the Fx+fmix64 `BuildHasher` is available (cheap swap), then measure.

## Do NOT self-host

- **The hashbrown table** inside `HashMap`/`HashSet` — std's is state-of-the-art; we cannot beat it. We only own the *hasher*.
- **`Vec`, `VecDeque`, `BTreeSet`** — correct, fast, idiomatic. No kevy-specific edge.

## Adjacent finding (not self-host; logged while measuring)

`Store::get` does ~3 keyspace lookups per hit (`reap` = `map.get` + `contains_key`,
then `map.get` again). Measured get_hit (124 ns) ≈ 3× get_miss (44 ns). A
single-lookup get (evaluate expiry on the entry already fetched) should ~halve
hit cost — independent of the hasher, and a bigger lever than the hasher swap on
the hit path. Separate backlog item (TREE.md).

## Status ladder

- Tier 1: **DONE (v0.perf-2)** — `kevy-hash` shipped + wired into `Store`; topic-01 fixed.
- Tier 2.1 (value-type maps): **DONE (v0.perf-6)**.
- Tier 2.2 (integer conn maps) + Tier 3: evaluated, **benchmark-gated, not started** — each needs its own micro-bench before any code.
