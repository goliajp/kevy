# Changelog

All notable changes to kevy. The format is loosely
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); kevy's release
cadence is "tag when a Wave closes," not strict semver below v1.0.

## [Unreleased]

The `develop` branch's snapshot that became the `v1.0.0-rc` line.
Everything below is already on `develop`.

### Added — Wave 3: embedded + WASM + release plumbing

- **New crate `kevy-embedded`** ([`crates/kevy-embedded/`](crates/kevy-embedded/)):
  in-process Redis-compatible KV without the server/runtime. Optional
  AOF + snapshot persistence, optional eviction (all 8 policies from
  Wave 2), optional background TTL reaper thread (or caller-driven
  `Store::tick()` for WASM). Zero crates.io deps — depends only on
  `kevy-store` + `kevy-persist`. 16 unit tests + 2 examples.
- **`kevy-bytes` builds on `wasm32-unknown-unknown`** — `SmallBytes`
  now has a cfg-gated 32-bit `Heap` layout
  (`ptr + len(u32) + cap(u32) + pad + tag`) alongside the existing
  64-bit `ptr + len + cap_and_tag × usize` shape. 64-bit perf is
  unchanged (locked layout, release perf_gate budgets met).
- **`kevy-embedded` + transitive closure** compile clean for
  `wasm32-unknown-unknown` AND `wasm32-wasip1`. See
  [`docs/wasm.md`](docs/wasm.md) for browser / WASI / Cloudflare
  Workers walkthrough.
- **GitHub Actions CI** ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)):
  x86_64-linux + aarch64-darwin (M-series) test matrix, wasm32 cargo
  check, nightly miri on `kevy-map` + `kevy-bytes`, vs-valkey docker
  smoke. Release pipeline (`release.yml`) runs `cargo publish
  --dry-run` for every publishable crate in dependency order and
  drafts a GitHub release on `vX.Y.Z` / `-rcN` / `-betaN` tags.
- **v1.x stability commitment** in [`README.md`](README.md): persistence
  format, RESP wire protocol, public Rust API, CLI flags + env vars,
  TOML schema, eviction policy names + algorithms — all add-only
  across v1.x.

### Added — Wave 2: 防 OOM + 防数据丢

- **`maxmemory` + 8 eviction policies**
  (`noeviction` / `allkeys-{lru,lfu,random}` / `volatile-{lru,lfu,random,ttl}`).
  Sample-based with `maxmemory-samples = 5` (matches Redis); LFU uses
  log-scale increment with splitmix32-derived PRNG (no decay in v1.0).
  Per-entry weight cache + `ENTRY_OVERHEAD` constant give O(1)
  accounting on every mutation path. Unlimited mode (`maxmemory = 0`,
  the default) stays at its tuned hot-path budget.
- **Active TTL reaper** — `Store::tick_expire(samples, rounds)` runs
  Redis's `activeExpireCycle` per shard. The reactor calls it at the
  configured `[expiry].hz` (default 10 Hz / 100 ms) via the new
  `Commands::on_shard_tick` hook in `kevy-rt`. Lazy expiry still
  runs alongside.
- **`BGREWRITEAOF`** — `Aof::rewrite_from(&Store)` dumps current state
  to `<aof>.rewrite` as canonical SET/HSET/RPUSH/SADD/ZADD (+ PEXPIRE
  for TTL'd keys) and atomically `rename(2)`s over the live AOF. v1.0
  is synchronous (each shard blocks for its own rewrite); v1.x will
  incrementalise. Auto-triggered by the shard tick when the AOF grew
  ≥ `auto_aof_rewrite_percentage %` (default 100) above its size at
  the last rewrite AND is ≥ `auto_aof_rewrite_min_size` (default 64 MiB).
- **`appendfsync` wired from config** — `Always` / `EverySec` (default)
  / `No`. Existing fsync semantics in `kevy_persist::Aof` were
  already implemented; this commit just plumbs the choice from
  `cfg.persistence.appendfsync` through to the per-shard `Aof::open`.
- **Crash-safety contract** documented in
  [`MIGRATION-FROM-VALKEY.md`](MIGRATION-FROM-VALKEY.md): truncated
  AOF tails replay cleanly (covered by
  `aof_truncated_tail_is_tolerated_on_restart`), snapshot+AOF load
  order is snapshot-first / replay-second. Power-loss simulation
  harness at [`bench/crash-test.sh`](bench/crash-test.sh).
- **`MEMORY USAGE / STATS / DOCTOR / PURGE`** commands; `INFO memory`
  now surfaces live `used_memory`, `used_memory_peak`,
  `evicted_keys`, `maxmemory_human`.

### Changed

- `kevy_persist::Fsync` now derives `Debug` / `PartialEq` / `Eq`
  (Wave 3 #5 needed it for `Config::default()` to derive Debug).
- `kevy_persist::Aof` carries its own path + size estimates so
  auto-rewrite can compute the trigger threshold without `fstat()`
  per append.
- `kevy_rt::Commands` trait gained two hooks (default no-op):
  `on_shard_init(store)` lets per-shard config (e.g. maxmemory) land
  before the reactor starts; `on_shard_tick(store)` +
  `shard_tick_interval_ms()` drive the active TTL reaper at the
  configured cadence.
- `kevy_map::KevyMap` gained `iter_from_bucket(start)` for the
  eviction sampler's random-start window. Existing `iter()` unchanged.

### Fixed

- `kevy-embedded::Store::Drop` recovers from mutex poison so the
  final AOF flush always runs (a panic in some method during the
  session shouldn't strand the EverySec window's writes).
- Several clippy lints across `kevy-map` / `kevy-store` / `kevy-persist`
  / `kevy-embedded` (collapse `if let`, type alias for complex
  signatures, `.is_multiple_of`, `io::Error::other`) so CI's
  `cargo clippy --workspace -- -D warnings` runs clean on first push.

---

## [v1.0.0-w1] — 2026-05-28

Wave 1 close: config + ops + docs. See git tag for the full list;
headlines:

- New crate `kevy-config` — 0-dep TOML subset parser + Config schema.
- 13 ops commands: `INFO` / `CLUSTER * ` / `DEBUG SLEEP` / `WAIT` /
  `SHUTDOWN` / `CONFIG GET/SET/REWRITE/RESETSTAT` / `CLIENT *`.
- Top-level `README.md` + `MIGRATION-FROM-VALKEY.md` (94-cmd
  parity table).
- Code-quality rule: `src/*.rs ≤ 500 LOC` / `fn ≤ 50 LOC` codified
  as a project coding rule.

## [v0.1.1-deep-polish-rc] and earlier

Per-crate perf polish across `kevy-bytes` / `-hash` / `-map` /
`-resp` / `-ring` / `-store`. The five library crates reach noise-floor
parity or better vs the best open-source Rust / Go / C / C++
competitor at each workload.
