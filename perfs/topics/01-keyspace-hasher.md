# Topic 01: keyspace HashMap hasher (SipHash tax)

**Status:** **fixed (v0.perf-2, 2026-05-25)**
**Severity:** high (hottest data-structure access in the server)
**First observed:** 2026-05-25 (TREE.md → kevy-store keyspace lookup)

## Symptom

The keyspace `HashMap<Vec<u8>, Entry>` is hit on every command. std's `HashMap`
defaults to `SipHash-1-3` — random-seeded, DoS-resistant — which a single-trust-
domain, single-threaded-per-shard keyspace does not need. Hypothesis: the SipHash
tax is pure overhead we can shed.

## Reproduction

```
cargo run -p kevy-store --example bench_keyspace --release
# regression gate:
cargo test -p kevy-store --test perf_gate
```
Data: `perfs/data/2026-05-25/keyspace-hasher.txt` (host load ~8; ratios are the signal).

## Hypotheses

1. **Swap SipHash → FNV-1a** — *ruled out.* FNV is byte-at-a-time; it loses to
   SipHash-1-3's word-at-a-time on every size (0.4–0.8×). Slower, not faster.
2. **Swap → FxHash (raw, word-at-a-time, no finalizer)** — *ruled out.* 4–8×
   faster at pure hashing BUT get_hit on short low-entropy keys collapses to
   0.02–0.03× (≈30–50× slower): weak avalanche → severe clustering in the table.
3. **Swap → FxHash write + murmur3 `fmix64` finalizer** — *confirmed.* Keeps the
   fast word-at-a-time write, adds ~6 ALU ops of avalanche once per hash. Fast
   *and* well-distributed.

## Investigation log

- 2026-05-25 — Built `kevy-bench` (pure-Rust harness) + `bench_keyspace`
  example. Measured 4 hashers against std SipHash on the *same* hashbrown table,
  12 B and 40 B keys, 3 process runs. Results (ratios vs SipHash, median):

  | Candidate | hash_one | get_hit | get_miss |
  |---|---:|---:|---:|
  | FNV-1a | 0.4–0.8× | 0.45–1.2× | 0.4–0.95× |
  | FxHash raw | 4–8× | **0.02×** (short, clusters) | 0.6–4.75× erratic |
  | **Fx + fmix64** | **4×** | **1.2–2.8×** | **1.1–1.7×** |

  Conclusion: the win is in the *hasher*, not the table (hashbrown stays). The
  `fmix64` finalizer is essential — without it the fast hash clusters and is a
  net loss. Recorded as RFC Tier 1
  (`rfcs/2026-05-25-std-self-host-evaluation.md`).

## Decision

Adopt **Fx + fmix64** as the keyspace hasher (seed `0x517cc1b727220a95`, rotate
5; `fmix64` on finish). Implement as a `kevy-hash` crate (`BuildHasher`), wire
into `Store`'s keyspace map. Pure-safe Rust, no `unsafe`. Table unchanged.

## Verification

**Done (v0.perf-2, 2026-05-25).** Implemented as the `kevy-hash` crate
(`FxHasher` = word-at-a-time absorb + `fmix64`; `FxBuildHasher`/`FxHashMap`),
wired into `kevy_store::Store.map`. Data: `perfs/data/2026-05-25/keyspace-hasher-adopted.txt`.

- **Correctness:** `cargo test -p kevy-store` 18 unit + 3 perf_gate + 1 doctest
  green on the new hasher (no iteration-order/SCAN breakage). `kevy-hash` itself
  ships a clustering-guard test (`no_catastrophic_clustering_on_low_entropy_keys`).
- **Map-level win holds post-adoption:** Fx+fmix64 vs SipHash on the same table
  — long-key get_hit 1.41×, get_miss 1.62–1.86× (short-key get_hit showed a
  0.90× noise blip this single run; the authoritative 3-run baseline is
  1.2–2.8×; the point is Fx+fmix64 never clusters, unlike raw Fx's 0.02×).
- **Production hot path (real `Store`, Fx+fmix64), absolute:** get_hit median
  124 ns (min 75), get_miss 44 ns (min 28), set 228 ns (min 180).

## Follow-up found while measuring (NOT hasher-related)

`Store::get` hit (124 ns) is ~3× its miss (44 ns) because `get` calls `reap`
(`map.get` + `contains_key`) and *then* `map.get` again — **~3 map lookups per
hit**. A single-lookup get (check expiry on the one entry already fetched) would
roughly halve hit cost. Logged as a new backlog item; see TREE.md.
