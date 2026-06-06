# Changelog

All notable changes to kevy. The format is loosely
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); kevy's release
cadence is "tag when a Wave closes," not strict semver below v1.0.

## [v1.4.2] — 2026-06-07

Patch release rolling up the v1.4.1 follow-ups: an XREAD BLOCK bug fix,
two CI/release hardening jobs that catch the exact failure modes the
v1.4.0 → v1.4.1 sequence exposed, and a workspace-wide src/*.rs ≤ 500
LOC sweep (every production file now matches the CLAUDE.md house rule;
test files exempt per Rust community norm).

No public API breaks. New trait method `Commands::resolve_block_argv`
on `kevy-rt` is additive with a default body, so existing embedders
recompile unchanged.

### Fixed

- `XREAD BLOCK ms STREAMS key $` no longer hangs when an `XADD` lands
  during the park window. The previous implementation kept the literal
  `$` in the parked argv; the wake retry re-resolved `$` to the
  *post-`XADD`* `last_id`, so the just-added entry sat at the cursor
  and the read returned 0 rows (the conn timed out instead of
  receiving the entry it was supposed to be woken by). Park-time now
  rewrites each `$` to the stream's current `last_id` via a new
  `Commands::resolve_block_argv` hook, so the wake retry sees the
  original cursor and the freshly added entry. New regression test
  `xread_block_dollar_id_wakes` exercises the real `$` form;
  `xread_block_woken_by_concurrent_xadd` keeps documenting the
  explicit-ID variant. (ROADMAP task #10 / v2-7d known limitation,
  closed.)

### Added — CI / release plumbing

- `.github/workflows/ci.yml`: new `release-profile` job that runs
  `cargo test --workspace --release --lib --tests` on every push to
  `release/**` and `hotfix/**` branches. Catches release-only bugs
  (compiler eliminating a branch, sub-microsecond timings rounding
  to zero — the exact shape of the v1.4.0 SLOWLOG regression) at PR
  review time instead of inside the publish workflow.
- `.github/workflows/release.yml`: new `Publish chain self-check`
  step before the publish loop. Reads `cargo metadata --no-deps`,
  lists every workspace member whose `publish` field is unset, and
  diffs that set against the hand-maintained `for c in …` chain.
  Aborts on either side of the symmetric difference: a publishable
  crate not in the loop (the v1.4.0 release shipped without
  kevy-geo this way), or a name in the loop that isn't a publishable
  workspace member.

### Changed — internal refactor (no API surface)

- All production `src/*.rs` files now ≤ 500 LOC and every `fn` ≤ 50
  LOC, matching the CLAUDE.md house rule. Test files (`tests.rs`
  modules) are exempt per the Rust community norm and remain
  uncapped.
- New sibling modules carry the lifted-out code; each keeps its
  parent's `impl<C: Commands> Shard<C>` (or `impl Commands for
  KevyCommands`) so behaviour + call shape are unchanged:
  - `kevy-rt/src/exec_dispatch.rs` — `start_single` +
    `try_inline_local` + the new `park_blocked` /
    `post_write_housekeeping` / `dispatch_inline` helpers that bring
    `try_inline_local` from 106 LOC down to 35 LOC.
  - `kevy-rt/src/shard_tick.rs` — per-tick housekeeping
    (`apply_live_runtime_config`, `maybe_auto_rewrite_aof`).
  - `kevy/src/cmd_resolve.rs` — `KevyCommands::resolve`'s body as
    `kevy_resolve(args)` + a `route_for_verb(upper, args)` helper.
  - `kevy/src/dispatch_resp3.rs` — `try_resp3_overrides` + the four
    `emit_*_resp3` reply helpers.
  - `kevy-client/src/subscribe_io.rs` — `send_to` / `recv_remote` /
    `frame_to_event` / `classify` and the per-field reply unwraps.
  - `kevy-config/src/error.rs` — `ConfigError` enum + Display +
    Error impls; the public `kevy_config::ConfigError` path is
    unchanged.
  - `kevy-embedded/src/pubsub_bus.rs` — `BusEntry` + `PubsubBus`
    (the per-`Inner` channel/pattern registry).

### Tooling

- New end-to-end test `xread_block_dollar_id_wakes` in
  `crates/kevy/tests/blocking.rs` (now 12 tests).

## [v1.4.1] — 2026-06-06

Hotfix for v1.4.0's SLOWLOG threshold semantics under release-profile
builds. The v1.4.0 tag exists in git but never reached crates.io —
the release pipeline's `Verify tag builds (release profile)` job
failed in this exact case, and the publish step never ran. v1.4.1 is
the first published `1.4.x` artifact.

### Fixed

- `SLOWLOG`: `slowlog-log-slower-than 0` now records every command,
  including the sub-microsecond writes whose `Instant::elapsed().
  as_micros()` rounds to `0` under release-profile optimization.
  Previously the threshold check was `elapsed <= threshold → skip`,
  meaning a `threshold = 0` discarded the `elapsed == 0` row that
  release-profile SETs always produce. The fix is one line in
  `exec_slowlog.rs` (`<=` → `<`) and brings the behavior in line
  with Redis (`if (duration < slowlog_log_slower_than) return;`).
  Caught by the v1.4.0 release pipeline; covered by all four
  `slowlog_*` integration tests under `--release`.

## [v1.4.0] — 2026-06-06

RESP3 wire protocol + the full v2 command sprint: Streams (basic ops +
consumer groups + BLOCK), Geo, BLPOP/BRPOP, keyspace notifications,
SLOWLOG, cross-shard RENAME, CONFIG REWRITE-with-comments, reactor-
tuning knobs. The first release tagged through the new git-flow SOP.

### Added — RESP3

- `HELLO [protover [AUTH user pass] [SETNAME name]]`. `HELLO 3` flips
  the connection into RESP3 mode (per-conn `RespVersion`, threaded
  through every cross-shard `Op::Dispatch`). RESP2 stays the default
  and the hot-path measurements remain unchanged.
- RESP3-shaped replies migrated: `HGETALL` / `CONFIG GET` → Map,
  `SINTER` / `SUNION` / `SDIFF` → Set, `ZSCORE` / `ZINCRBY` → Double,
  `ZRANGE WITHSCORES` → nested `[bulk, double]`, `INFO` /
  `CLIENT INFO|LIST` → Verbatim string, `(P)SUBSCRIBE` message
  frames → Push (`>`). `RESP2` replies for the same commands are
  unchanged.
- `kevy-client`: RESP3 Push-frame demux + `Subscriber::hello3()` so
  embedders can negotiate RESP3 from a clean async API.

### Added — Streams (v2-7)

- Basic ops: `XADD` / `XLEN` / `XRANGE` / `XREVRANGE` / `XDEL` /
  `XTRIM` / `XREAD`. New `Value::Stream(Box<StreamData>)` keeps the
  Value enum at 32 bytes — the indirection only pays on stream
  operations.
- Consumer groups: `XGROUP CREATE|SETID|DESTROY|CREATECONSUMER|
  DELCONSUMER`, `XREADGROUP`, `XACK`, `XPENDING`, `XCLAIM`,
  `XAUTOCLAIM`. PEL stored in a `BTreeMap<StreamId, PelEntry>` so
  `XPENDING start end` is `O(log n + k)`; per-consumer `pel_count` is
  maintained on every PEL mutation so `XINFO` runs in O(group size).
- `XINFO STREAM | GROUPS | CONSUMERS | HELP`.
- `t`-class keyspace notifications (matches Redis): XADD / XDEL /
  XTRIM / XGROUP* / XACK / XCLAIM / XAUTOCLAIM / XREADGROUP all fire
  their lowercased verb name. The `A` flag includes the `t` class,
  matching modern Redis.
- AOF rewrite for streams: one `XADD` per entry (correct but linear
  in stream size — documented for now). RDB has a dedicated
  `OP_STREAM = 6` opcode carrying the full scalar state
  (`last_id`, `max_deleted_id`, `entries_added`).

### Added — BLOCK reactor (v2-7d)

- Per-shard `BlockedClients` registry shared by `BLPOP` / `BRPOP` /
  `XREAD BLOCK` / `XREADGROUP BLOCK`. FIFO per key (Redis arrival
  order), secondary index by conn for O(1) cleanup on close. Empty
  in steady state so the wake / tick hot paths short-circuit on
  `is_empty()`.
- New `Commands::block_hint(args) -> BlockHint` trait method (default
  `None`), folded into `ResolvedCmd { block_hint, wake_idx }` so the
  verb table is scanned once per command. The reactor's wake hook
  fires only when `wake_idx` is `Some` *and* `BlockedClients` is
  non-empty — so the steady-state cost of the registry on a
  no-block workload is one `is_empty()` check per write.
- `BLPOP key timeout` / `BRPOP key timeout` (single-key form). Empty
  list parks the conn; a sibling `LPUSH` / `RPUSH` wakes the oldest
  waiter and replays the command. Multi-key form returns an explicit
  cross-shard error (v2-7e will lift the same-shard subset).
- `XREAD BLOCK ms STREAMS key id` / `XREADGROUP GROUP g c BLOCK ms
  STREAMS key >`: same-shard waiter on the first STREAMS key, woken
  by an `XADD` to that key. `XREADGROUP BLOCK` only parks for
  `>`-mode streams (matches Redis).
- 11 end-to-end blocking tests against a real reactor + socket
  (hit / timeout / wake per command).

### Added — Geo (v2-6)

- `GEOADD` / `GEOPOS` / `GEODIST` / `GEOHASH` — stored as a ZSet with
  a 52-bit interleaved geohash for the score. `GEOHASH` emits the 11-
  char base32 form (the 11th char carries an IEEE-754 LSB drift; the
  first 10 chars match Redis exactly).
- `GEOSEARCH FROMLONLAT|FROMMEMBER BYRADIUS|BYBOX` + the legacy
  `GEORADIUS` / `GEORADIUSBYMEMBER` family + `GEOSEARCHSTORE`. All
  share one `run_search` core using 9-cell neighbor pruning plus
  exact Haversine secondary filtering.

### Added — Ops + config (v2-1 → v2-5)

- Keyspace notifications: per-shard `NotificationFlags`, hot-reloaded
  from the `[notify]` config section (`notify-keyspace-events Kg$`-
  style flag string). Single-key writes notify in the `try_inline_
  local` fast path; multi-key writes route through dedicated
  `maybe_notify_*` hooks.
- `[advanced]` config section (`spin_limit` / `park_timeout_ms` /
  `tick_check_every`) — the old hardcoded SPIN_LIMIT / PARK_TIMEOUT_
  MS / TICK_CHECK_EVERY constants are now per-shard fields, threaded
  through `Runtime::with_advanced`. Defaults match the pre-v1.4 hot
  numbers.
- `RENAME` / `RENAMENX` cross-shard orchestrator using
  `take_with_ttl` + `put_with_ttl` (same-shard atomic still goes
  through one `Store::rename`).
- `SLOWLOG GET | LEN | RESET | HELP` — bounded ring of slow
  command records per shard, hot-reloaded from
  `[slowlog].slower_than_micros` + `[slowlog].max_len`. SLOWLOG OFF
  (default) skips the clock pair entirely on the hot path.
- `CONFIG REWRITE` now preserves comments + key ordering (line-by-
  line rewrite, not a syntax-tree rebuild; missing sections get
  inline-appended).

### Changed

- `kevy-rt::Commands::resolve` now produces a `ResolvedCmd` with two
  new fields: `block_hint: BlockHint` and `wake_idx: Option<u8>`.
  **Breaking** for any external `impl Commands for X` that
  constructs a `ResolvedCmd` literal — add the two fields. The
  default `resolve()` implementation (which calls the per-attribute
  methods one-by-one) does so automatically.
- `BlockHint` / `BlockKind` re-exported from `kevy-rt` so concrete
  command-set crates (kevy + future ports) can return blocking
  classifications without taking a kevy-rt-internal dependency.
- Reply ordering: `Conn.blocked: bool` gates command dispatch on
  parked conns; the reactor stops parsing further commands on a
  conn while it's blocked, resumes on wake / timeout.
- CI workflows: `ci.yml` triggers expanded from `[main, develop]`
  (the `main` branch never existed in this repo) to `[master,
  develop, feature/**, release/**, hotfix/**, bugfix/**,
  support/**]` — feature branches now run CI on every push so
  Linux-specific build issues are caught before the merge.
- `master` is now the v1.3.0 ref (was: initial commit). All v1
  tags previously landed on `develop`; future releases follow the
  git-flow SOP and tag on `master` via `release/*` branches.

### Fixed

- `io_uring` reactor compile-clean on Linux:
  `crate::shard::TICK_CHECK_EVERY` was renamed to a per-shard field
  (`self.tick_check_every`) in v1.4 (advanced config), and the
  io_uring path's `Inbound::RequestBatch` drain was missing the
  `RespVersion` argument that v2-7 added to `Op::Dispatch`. macOS
  builds didn't notice because the io_uring path is
  `#[cfg(target_os = "linux")]`. CI now covers Linux on every push.

### Tooling

- New `GIT-FLOW.md` codifies the feature / release / hotfix flows
  including the v2-7d retro lessons (push the feature branch once,
  squash-merge on finish, bump versions on release branches only).
- New `.githooks/pre-commit` rejects any commit whose staged
  `crates/*/src/**/*.rs` blob exceeds 500 LOC (test files exempt).
  Set up via `bash .githooks/install.sh`, which also wires
  `gitflow.feature.finish.squash = true`.
- New `crates/kevy/tests/blocking.rs` — 11 end-to-end blocking
  tests for BLPOP / BRPOP / XREAD BLOCK / XREADGROUP BLOCK.

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
