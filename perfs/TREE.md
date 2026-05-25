# kevy component map

Where time goes, per crate. Each hot path is ✓ healthy / · informational /
⚠ (links to a topic). Updated 2026-05-25.

Conditions for the numbers below: M-series Mac, release, `kevy-bench` medians,
**ratios** (host was loaded ~8, so absolutes are indicative only).

## kevy-store — the keyspace (hottest crate)

| Path | Structure | Status |
|---|---|---|
| keyspace GET (string) | `FxHashMap` + `live_entry` | ✓ [topic-01](topics/01-keyspace-hasher.md)+[topic-02](topics/02-keyspace-read-path.md) **fixed**: Fx+fmix64 hasher + single-lookup/clock-skip → **get_hit ~28 ns, miss ~12 ns** |
| INCR/APPEND/GETSET/GETDEL/EXISTS | `live_entry`/`live_entry_mut` | ✓ topic-02 (v0.perf-4): one lookup + clock-skip → **incr_by ~80 ns** |
| typed reads/writes (HGET/HSET/LINDEX/LPUSH/SADD/Z*) | per-type `*_ref`/`*_mut` → `live_entry`/`live_entry_mut` | ✓ v0.perf-7 (topic-02): all 8 helpers migrated; one lookup + clock-skip |
| keyspace SET (string) | `live_entry_mut` overwrite-in-place | ✓ [topic-03](topics/03-set-overwrite-clone.md) **fixed** (v0.perf-5): no key re-clone on overwrite + clock-skip → **~130→~70 ns (~1.8–2×)** |
| typed writes (HSET/LPUSH/SADD/ZADD) | `reap` + `get_mut`/insert | · same overwrite-clone + reap pattern; follow-up |
| list ends | `VecDeque` | ✓ O(1) ends, right structure |
| zset by-score | `BTreeSet<(Score,member)>` | ✓ std B-tree, keep |
| hash/set/zset value types | `kevy_hash::FxHashMap`/`FxHashSet` | ✓ v0.perf-6 (RFC Tier 2.1): Fx+fmix64, win transfers from keyspace |

## kevy-rt — runtime / reactor

| Path | Structure | Status |
|---|---|---|
| conn lookup per event | `HashMap<u64,Conn>` / `<i32,u64>` SipHash | · RFC Tier 2 (integer key → fmix64 or slot map); not yet measured |
| io_uring conn map | `HashMap<u64,UringConn>` | · same |
| cross-shard scatter | `HashMap<usize,_>` by_shard | · RFC Tier 3 (`Vec`-indexed); trivial |
| set-ops gather | `HashSet<&Vec<u8>>` | · RFC Tier 3 |
| per-conn reply ordering | `VecDeque<PendingSlot>` (seq-ring) | ✓ |
| cross-core messaging | `kevy-ring` lock-free SPSC | ✓ (measured perf-neutral vs old, syscall already gone) |

## kevy-resp / kevy-sys / kevy-net / kevy-persist / kevy-ring

| Crate | Note | Status |
|---|---|---|
| kevy-resp parse_command | `Vec<Vec<u8>>` owned argv | · v0.perf-8 measured: SET ~70 ns, **alloc-bound** (4 allocs, owned for cross-core). encoders near-free (~2–5 ns). Single-alloc-argv win deferred (Command-type change, big blast radius). perf_gate + BUDGETS added |
| kevy-sys | libc boundary (sockets/epoll/io_uring) | · syscall-bound, not algorithmic |
| kevy-ring | lock-free SPSC ring | ✓ has its own tests |
| kevy-persist | RDB/AOF (background / startup) | · not hot path |
| kevy-net | blocking reactor `HashMap<i32,Conn>` | · legacy path, low priority |

## Macro (full-system) — blocked

| Metric | Tool | Status |
|---|---|---|
| `-c1` / `-c50` throughput vs valkey/redis | `bench/bench3.sh` | ⚠ blocked on idle host (busy-poll starves under load) |
| pub/sub fan-out throughput | `bench/pubsub_bench.sh` + `kevy-loadgen` | ⚠ same |
| compatibility | `bench/compat3.sh` | ✓ 61/61 vs valkey 9.1 AND redis 7.4 |
