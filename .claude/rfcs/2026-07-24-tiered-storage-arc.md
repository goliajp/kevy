# RFC — transparent hot/cold tiering: a memory budget with disk overflow

**Date:** 2026-07-24 · **Status:** DESIGN ROUND — user vision received
("透明冷热数据…配置或自动探测环境,只允许最大占用的内存值,其他的要允许在
硬盘上,否则大容量的存储我们根本做不了"). This RFC turns it into a staged,
zero-tax path. **Nothing here is implementation-started**; open decisions
at the end are the 拍板 surface. Sibling: `2026-07-24-virtual-rds-views-arc.md`.

User constraints: transparent under Redis semantics (every op still works
on a cold key); memory budget by config **or auto-detected**; additive —
mainline performance and logic untouched. The design below meets all
three, and the "untouched" claim is structural, not aspirational: **the
hot path gains zero instructions** (the cold check lives on the map-miss
path, which is already the slow path).

## 1. What exists (surveyed 2026-07-24, file:line grounded)

- Read funnel: `Store::live_entry`/`live_entry_mut`
  (`kevy-store/src/accounting.rs:171,204`) — all 66 typed-read call sites
  pass through; after it, ~196 `Value::` match sites across 22 files.
  ⇒ **rehydration must happen at the funnel** so every op stays oblivious;
  a new `Value::Spilled` variant is ruled out (would touch 196 sites and
  break the `size_of::<Value>() ≤ 32` assert, `value.rs:218`).
- Eviction: sample-based, runs reactively on write
  (`try_evict_after_write`, `lib.rs:449`); `evict_one`
  (`evict.rs:128-141`) picks the coldest sampled victim and
  **deletes it** (`remove_entry`). The whole policy/sampling/O(1)
  accounting substrate (Entry.weight, lru_clock, 8 policies) is a
  ready-made **demotion** picker — the fork is one function.
- Values: `Str` inline ≤22 B / `ArcBulk` >64 B / `Int` / Arc-wrapped
  collections (frozen-snapshot clone via Arc — the same trick snapshots
  use, exploitable by a background spill writer; `is_heap_heavy` ≥4 KiB +
  `bio_drop` is the existing off-thread handoff precedent).
- AOF: v2 `[len][crc32c][payload]` envelope — seekable in principle, but
  no per-value offset index exists anywhere. kevy-sys has **no
  pread/pwrite/mmap**; `std::os::unix::fs::FileExt::read_at` is available
  (std, 0-dep preserved — AOF already uses `std::fs`).
- Memory auto-detection: **none exists** (grep-verified).
- Zero-cost-off house pattern: `Option<Positions>` physical bypass /
  `maxmemory == 0` single not-taken branch.

## 2. The design — keydir in RAM, values in a disposable value log

**Shape (Bitcask/WiscKey genre, adapted to kevy's AOF-is-truth reality):**
keys + metadata stay in RAM permanently; cold **values** live in an
append-only value log (vlog) on disk; hot values stay in RAM exactly as
today. Capacity becomes: RAM bounds the *keyspace* (~100 B/key incl.
key bytes), disk bounds the *data*.

### 2.1 The load-bearing simplification: the vlog is NOT durability

The AOF remains the sole durable truth — untouched. The vlog is a
**disposable spill area, rebuilt on every boot**: opening a store deletes
any stale vlog and replays the AOF as today, except with tiering active
the replay itself demotes past-budget values straight to a fresh vlog.
Consequences:
- **Zero new crash-safety surface.** No vlog recovery, no vlog fsync
  policy, no torn-write handling (a CRC per record guards read-back of
  what we ourselves wrote this boot; a bad read is a bug, not corruption
  to heal). The durability-trust arc (t5.5) is not touched.
- Boot cost: replay already reads every value; writing the cold share to
  the vlog adds sequential writes at replay speed. Honest, measured in
  the gate.
- Snapshot/rewrite machinery unchanged (they read via the same funnel;
  a rewrite of a mostly-cold store will page values in — streamed, and
  the rewrite reads values one at a time; noted as a T5 measurement).

### 2.2 Data structures (per shard)

- `tier: Option<TierBackend>` on `Store` — `None` (default) = today's
  binary, the `Option<Positions>`/`maxmemory==0` precedent.
- **Cold side-table**: `KevyMap<SmallBytes, ColdRef>` where
  `ColdRef = { vlog_id: u32, offset: u64, len: u32, expire_at, weight,
  lru_clock }` (~40-48 B/cold key). Cold keys are **removed from the main
  map** — so a hot GET is byte-identical to today (main-map hit, not even
  a branch), and only a **miss** consults the cold table before returning
  nil. SCAN/KEYS/DBSIZE/TYPE/RANDOMKEY iterate both tables (Law 1:
  transparent; cold keys are visible keys).
- **vlog**: per-shard append files (`vlog-<shard>-<n>`), record =
  `[len][crc32c][value bytes]` (the AOF envelope genre), read by
  `File::read_at`, rotated at a size threshold; compaction = when a
  file's live ratio (tracked by the cold table) drops below a threshold,
  copy live records forward and delete it — mirroring the AOF
  auto-rewrite trigger discipline.

### 2.3 The two hooks

- **Demote** (`evict_one` fork): when `tier` is Some and the policy is a
  `tiered-*` policy, the sampled victim's value is appended to the vlog
  and the key moves main-map → cold-table (RAM freed = value weight;
  `ENTRY_OVERHEAD` mostly stays — honest accounting: tiering frees value
  bytes, not key bytes). Heavy values (≥4 KiB, `is_heap_heavy` precedent)
  can hand the frozen Arc to a background spill thread (bio genre) so the
  reactor never blocks on the write; small values spill inline (cheap).
  v1 scope: `Str`/`ArcBulk`/`Int` values spill; **collections stay hot**
  (spilling a live Hash/ZSet needs serialization + partial-op semantics —
  deliberately v2, stated honestly; the big-capacity workload is
  dominated by scalar blobs).
- **Rehydrate** (map-miss path in `live_entry`/`live_entry_mut` + the
  write funnels): miss → cold-table hit → `read_at` the value (CRC
  check) → either **promote** (re-insert into main map, default: yes,
  stamped hot; may immediately re-demote something else — hysteresis:
  promotion only bumps the entry to probationary recency, the existing
  lru_clock machinery already gives this shape) or serve-without-promote
  for point reads under pressure (policy knob). Writes to a cold key
  (SET/DEL/EXPIRE/type ops): invalidate the cold ref (dead bytes await
  compaction), operate in RAM as today.

### 2.4 Budget + auto-detection

- Config (both surfaces — kevy-config TOML/CLI/env AND the embedded
  builder): `[tiering] budget = "4gb" | "auto" | "70%"`,
  `spill_dir`, `min_spill_bytes` (don't spill tiny values; default ~64 B
  = BULK_THRESHOLD), `promote_on_read`, policy = `tiered-lru |
  tiered-lfu` (reusing the sampling machinery; the existing 8 delete-
  policies keep their exact semantics — tiering is a new policy family,
  not a change to eviction).
- **Auto**: Linux = cgroup v2 `memory.max` (container-honest) falling
  back to `/proc/meminfo MemAvailable`; macOS = `sysctlbyname
  "hw.memsize"` via a hand-bound kevy-sys extern (the sanctioned OS
  boundary). Default fraction ~70% of the detected bound. Re-probed on
  the shard tick so cgroup changes are honored (same live-reapply
  discipline as maxmemory today, commands.rs:247).
- Relationship to `maxmemory`: `maxmemory` keeps meaning what Redis means
  (hard cap + delete-eviction). The tier budget is the *demotion*
  watermark. Running both is legal (demote at budget, delete at
  maxmemory as the backstop). INFO grows a tiering section (cold keys,
  vlog bytes, live ratio, promotions/demotions, cold-read p99).

### 2.5 Performance strategy (the user's named main challenge)

- **Hot path: zero added instructions** — hot GET/SET = main-map hit =
  today's code exactly; the cold check is on the miss path. The claim is
  gated: perfgate must show byte-identical throughput with tiering off
  AND with tiering on + working set fitting in budget.
- **Cold read**: one `read_at` ≈ 5–30 µs NVMe — honest, measured; the
  serving thesis is that a serving working set is hot, and cold reads are
  the tail you accept for 10× capacity. io_uring-async cold reads (so a
  cold GET doesn't stall the shard's event loop) are named as the v2
  perf train — v1 does the pread synchronously on the shard (documented).
- **Demotion**: off-thread for heavy values (bio precedent), inline for
  small; sampling cost is the existing eviction cost.
- **tiergate** (new): hot-hit non-regression / cold-read p99 budget /
  demotion throughput / boot-replay-with-spill time / compaction
  amplification. Competitive axis (later train): kvrocks (RocksDB-based
  redis) and redis-on-flash-genre — the honest "big-capacity" rivals.

## 3. Law/charter check

- Law 1: every Redis op works on cold keys (transparent); cold keys are
  ordinary keys to SCAN/TYPE/TTL. No new verbs needed for v1 beyond INFO
  fields (maybe `TIER.STATS` later).
- 0-dep: std::fs read_at + hand-bound sysctl in kevy-sys — no crates.io.
- Durability contract (t5.5): untouched — AOF semantics, fsync classes,
  crash guarantees all identical; the vlog is explicitly non-durable
  state, deleted on boot.
- wasm/mem:// : tiering unavailable (no disk) — config rejected cleanly.

## 4. Trains (linear, each five-axis gated)

1. **T1 — `kevy-vlog` stone**: append/read_at/rotate/compact + CRC +
   unit/fuzz + bench (pure lib, no Store coupling).
2. **T2 — Store tier hooks**: cold side-table, evict_one fork,
   miss-path rehydrate, write-invalidate; server + embedded; the
   zero-tax perfgate proof is this train's headline gate.
3. **T3 — budget config + auto-detect** (both config surfaces; cgroup/
   meminfo/sysctl probes in kevy-sys).
4. **T4 — replay-time demotion** (boot within budget) + INFO/observability.
5. **T5 — tiergate + docs** (durability note: what the vlog is and is
   not) + lx64 envelope numbers (e.g. 100M keys × 4 KiB on a 8 GiB
   budget box).
6. **T6 — v2 items** (each its own decision): collection spill,
   io_uring async cold reads, `TIER.STATS` verb, competitive bench vs
   kvrocks.

## 5. Open decisions (拍板 surface)

1. **Arc ordering**: tiering vs virtual-RDS-views first (independent
   arcs; tiering touches the store stone — highest blast radius — and
   likely wants to land before views work multiplies read paths).
2. v1 scalar-only spill (collections stay hot) — acceptable staging?
3. Policy surface: new `tiered-lru/tiered-lfu` policy names (proposal)
   vs a separate `[tiering]` on/off orthogonal to maxmemory-policy.
4. `promote_on_read` default (proposal: yes, with probationary recency).
5. Whether T5's envelope target (the "大容量" headline number) should be
   sized to a concrete consumer workload — 拍板 what capacity story we
   advertise (keys × value size × RAM budget).
