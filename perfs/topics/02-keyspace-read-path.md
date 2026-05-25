# Topic 02: redundant work on the keyspace read path

**Status:** **fixed (v0.perf-3, 2026-05-25)** — string GET; typed reads follow-up open
**Severity:** high (GET is the headline hot path)
**First observed:** 2026-05-25 (while measuring topic-01; `data/2026-05-25/keyspace-hasher-adopted.txt`)

## Symptom

The real `Store::get` hit (~124 ns) is ~3× its miss (~44 ns). Two redundancies:

1. **Triple lookup on hit.** The old `get` was `reap(now)` then `map.get`. `reap`
   does `expired()` (`map.get`) + `contains_key` (`map.get`); then `get` does a
   third `map.get`. Three keyspace probes per hit.
2. **Unconditional clock read.** `get` called `Instant::now()` (~20–40 ns on
   macOS) on *every* call to drive lazy expiry — but **most keys carry no TTL**
   (`expire_at == None`), so the clock read is pure waste on the common path.

## Reproduction

```
cargo run -p kevy-store --example bench_keyspace --release   # "real Store" section
cargo test -p kevy-store --test perf_gate
```
Data: `perfs/data/2026-05-25/keyspace-hasher-adopted.txt` (baseline: get_hit ~124 ns,
get_miss ~44 ns, set ~228 ns).

## Hypotheses

1. **Drop one lookup** by fusing the expiry check and the value fetch into one
   read path → expect ~1 fewer probe on hit, ~1 on miss.
2. **Skip the clock when there's no TTL** → expect ~20–40 ns off every no-TTL hit
   (the common case). *This is the bigger lever.*

## Decision

Add `Store::live_entry(&mut self, key) -> Option<&Entry>`: a single peek decides
expiry, reading `Instant::now()` **only if `expire_at.is_some()`**; expired keys
are dropped (same semantics as `reap`); live keys are returned. Two-phase
(decide, then mutate/fetch) so the borrow checker is satisfied without an owning
key clone. `get` now delegates to it. Lazy-expiry semantics are unchanged
(expired key on read → removed + `None`).

Scope: `get` (string GET) this checkpoint. The same `reap(now)`-then-access
pattern in the typed reads (HGET/LINDEX/SISMEMBER/Z*) and `exists` is a
follow-up migration to `live_entry` / a `_mut` variant.

## Verification

**Done (v0.perf-3).** `data/2026-05-25/keyspace-read-path.txt` (3 runs).

| Op | Before (adopted.txt) | After (3-run) | Note |
|---|---:|---:|---|
| `Store::get` hit | ~124 ns | **~28 ns** | |
| `Store::get` miss | ~44 ns | **~12 ns** | |
| `Store::set` (control, unchanged) | ~228 ns | ~135 ns | host was calmer this run |

The host was less loaded during the "after" run — `set` (which I did **not**
change) dropped 228→135 ns (×0.59), so part of the raw get drop is load, not the
fix. **Load-normalised via the SET control, the optimisation itself is ~2.2–2.6×**
on get_hit (124×0.59 ≈ 73 → 28) and get_miss. Direction and magnitude are clear:
removing one keyspace lookup and skipping `Instant::now()` on no-TTL keys is a
large, real win on the headline read path. (This is exactly why the methodology
prefers in-process back-to-back ratios; absolute cross-run numbers drift with
load, so the SET control is used to normalise.)

Correctness: `cargo test -p kevy-store` 18 unit (incl. expiry) + 3 perf_gate + 1
doctest green; lazy-expiry semantics unchanged.

## Extended (v0.perf-4, 2026-05-25): string + generic read-modify paths

Added `Store::live_entry_mut` (mutable twin: one lookup, clock only on TTL'd
keys) and migrated the rest of the `reap(now)`-then-access pattern in string +
generic commands: `incr_by` (INCR/INCRBY/DECR/DECRBY), `incr_by_float`,
`append`, `getset`, `getdel`, and `exists` (now reuses `live_entry`). `incr_by`
went from ~4 keyspace lookups + an unconditional clock read to one
`live_entry_mut` (+ clock only if TTL'd), mutating the value in place and
preserving any TTL.

Numbers (`data/2026-05-25/keyspace-incr-path.txt`, 3 runs, SET ~130 ns control
matching the v0.perf-3 conditions): **`Store::incr_by` ~80 ns**, get_hit ~29 ns,
get_miss ~13 ns. No clean pre-migration INCR number was captured, but the path
dropped from ~4 lookups + clock to 1–2 lookups + conditional clock — estimated
~1.5–2× on the INCR path. kevy-store 18 unit (incl. INCR + expiry) + 3 perf_gate
+ doctest green.

## Typed commands (v0.perf-7, 2026-05-25) — DONE

The 8 central per-type accessor helpers — `hash_ref`/`hash_mut`,
`list_ref`/`list_mut`, `set_ref`/`set_mut`, `zset_ref`/`zset_mut` — were the
single chokepoint every typed command (HGET/HSET/LPUSH/LRANGE/SADD/SISMEMBER/
ZADD/Z*) routes through, and each carried the same `reap(now)`-then-access
pattern. Migrated all 8 to `live_entry`/`live_entry_mut` (removed the now-unused
`Instant` imports in list/set/zset). All typed commands now inherit the
one-lookup + clock-skip read path. kevy-store 18 unit + 4 perf_gate + doctest +
kevy sharded 11/11 green; clippy 0 workspace-wide.

**The read-path redundancy is now eliminated across every command family**
(string/generic in topics 02/03; hash/list/set/zset here).
