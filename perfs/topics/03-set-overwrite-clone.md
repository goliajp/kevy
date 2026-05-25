# Topic 03: SET re-clones the key on overwrite

**Status:** **fixed (v0.perf-5, 2026-05-25)**
**Severity:** high (SET is a headline op)
**First observed:** 2026-05-25 (noted in topic-02 / TREE while measuring the read path)

## Symptom

`Store::set` was `reap(now)` then `self.map.insert(key.to_vec(), …)`. On an
**overwrite** (the common case — hot keys, and what the benchmark measures) the
key already lives in the table, so std `HashMap::insert` keeps the existing key
and **drops the freshly-allocated `key.to_vec()`** — a wasted heap allocation
every SET. Plus `reap` read `Instant::now()` unconditionally even for plain SET
(no EX/PX).

Baseline: `Store::set` overwrite ~130 ns (`data/2026-05-25/keyspace-incr-path.txt`).

## Reproduction

```
cargo run -p kevy-store --example bench_keyspace --release   # "real Store" section
cargo test -p kevy-store --test perf_gate
```

## Decision

Restructure `set` around `live_entry_mut`:

- compute `expire_at` first, reading `Instant::now()` **only if** EX/PX given;
- `Some(e)` (exists & live) → NX aborts, else overwrite `e.value` + `e.expire_at`
  **in place — no `key.to_vec()`**;
- `None` (absent / expired-and-dropped) → XX aborts, else `insert(key.to_vec(), …)`.

NX/XX/clear-TTL semantics preserved (SET without EX clears the TTL by setting
`expire_at = None`).

## Verification

`data/2026-05-25/keyspace-set-path.txt` (3 runs; `incr_by` ~63–90 ns as the
unchanged control, matching the v0.perf-4 run so conditions are comparable):

| Op | before | after |
|---|---:|---:|
| `Store::set` overwrite | ~130 ns | **~56–84 ns (median ~70)** |

**~1.8–2× on overwrite SET**, from dropping the wasted key allocation + the
unconditional clock read. Correctness: kevy-store 18 unit (incl. SET/NX/XX/EX +
expiry) + 4 perf_gate + doctest green; kevy end-to-end (commands + sharded) green.

## Note

First-insert (key absent) is unchanged (still one `insert` with `key.to_vec()`),
plus a cheap `live_entry_mut` miss — negligible, and rarer than overwrite on hot
keys.
