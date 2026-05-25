# kevy-store performance budgets

Regression budgets in `tests/perf_gate.rs`. Run `cargo test -p kevy-store --test perf_gate`.
Exploration / self-host evidence in `examples/bench_keyspace.rs`:
`cargo run -p kevy-store --example bench_keyspace --release`.

## Path taxonomy

The keyspace `HashMap<Vec<u8>, Entry>` is on **every** command — GET/SET/DEL hit it
directly, and every typed command (HGET, LPUSH, SADD, ZADD…) first looks the key up
there. So the per-lookup cost multiplies across the entire workload; it is the single
hottest data-structure access in the server.

- **get (hit)** — GET, and the read half of every typed command. Hottest path.
- **get (miss)** — negative lookups: SETNX, EXISTS-on-absent, first write to a key.
- **set (overwrite/insert)** — SET and the write half of typed commands. Clones the
  key (`key.to_vec()`) and boxes the value, so it carries allocation cost on top of
  the hash+probe.

## Measured (M-series Mac, release, kevy-bench median; ratios vs SipHash)

From `examples/bench_keyspace.rs`, isolating the **hasher** (table held constant as
std's hashbrown). See `rfcs/2026-05-25-std-self-host-evaluation.md` Tier 1.

| Op | SipHash (std default) | FNV-1a | FxHash raw | Fx + fmix64 |
|---|---:|---:|---:|---:|
| hash_one, 12 B | ~4 ns | 0.8× | 4× (clusters) | **4×** |
| hash_one, 40 B | ~8 ns | 0.4× | 8× (clusters) | **4×** |
| get_hit | ~13–20 ns | 0.45–1.2× | 0.02–1.07× | **1.2–2.8×** |
| get_miss | ~16–24 ns | 0.4–0.95× | 0.6–4.75× erratic | **1.1–1.7×** |

Decision: adopt **Fx + fmix64** (fast word-at-a-time write + murmur3 avalanche).
FNV (byte-at-a-time) and raw Fx (no finalizer → clustering) were measured and rejected.
**Adopted (v0.perf-2):** `Store.map` now uses `kevy_hash::FxHashMap`.

### Production hot path (real `Store`, Fx+fmix64 + `live_entry`, release)

| Op | v0.perf-2 | v0.perf-3 | note |
|---|---:|---:|---|
| `Store::get` hit | ~124 ns | **~28 ns** | topic-02: single lookup + clock-skip (load-normalised ~2.2–2.6× of the drop) |
| `Store::get` miss | ~44 ns | **~12 ns** | topic-02 |
| `Store::set` overwrite | ~228 ns | **~70 ns** (v0.perf-5) | no key re-clone on overwrite + clock-skip (~1.8–2× vs the ~130 ns v0.perf-3 figure) |
| `Store::incr_by` | — | **~80 ns** (v0.perf-4) | one `live_entry_mut`; was ~4 lookups + clock |

`get`/`incr_by`/`append`/`getset`/`getdel`/`exists` now use
`Store::live_entry`/`live_entry_mut` (one keyspace probe; `Instant::now()` read
only when the entry has a TTL). The same `reap`-then-access pattern still lives
in the **typed** reads (HGET/LINDEX/…) — topic-02 follow-up. SET overwrite still
re-clones the key — a candidate next.

## Regression gates

`tests/perf_gate.rs`: `get_hit`, `get_miss`, `set_overwrite`, `incr` — each
asserts under a loose µs budget (catches order-of-magnitude regressions).

## Regression budgets (`tests/perf_gate.rs`, dev profile, generous headroom)

| Path | Budget | Observed (dev, loaded) | Headroom |
|---|---:|---:|---:|
| `get_hit` | 10 µs | ~150–600 ns | ~15–60× |
| `get_miss` | 10 µs | ~150–600 ns | ~15–60× |
| `set_overwrite` | 20 µs | ~300 ns–1 µs | ~20–60× |

Budgets are deliberately loose: they catch **order-of-magnitude** regressions (a bad
hasher swap, an accidental O(n) probe walk, a stray per-op allocation), not ns drift.
The dev profile is unoptimised (~5–25× slower than release) and the host is shared.

## When to re-measure

- Swapping the keyspace hasher (the Tier-1 change) — expect get_hit/get_miss to
  *improve*; the gate must stay green and the bench must confirm the ratio.
- Changing `Entry` layout / value boxing (would move the `set` allocation cost).
- Adding background expiry (today expiry is lazy on access via `reap`).
