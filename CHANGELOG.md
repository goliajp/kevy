# Changelog

All notable changes to kevy. The format is loosely
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); kevy's release
cadence is "tag when a Wave closes," not strict semver below v1.0.

## [v1.27.2] — 2026-06-24 (test serialization fix for v1.27.1 cache change)

v1.27.1 ship caught by CI verify on the lx64 runner: the v1.27.1
process-global SCRIPT cache (which is correct production behaviour)
means `SCRIPT FLUSH` from one test in `tests/lua_eval.rs` wipes
scripts loaded by other tests in the same binary. Local Mac dev's
lighter parallelism let this slide; lx64's heavier scheduler
surfaced `evalsha_ro_blocks_write_in_cached_script` failing because
another concurrent test flushed mid-LOAD-then-EVALSHA.

Fix: same `script_cache_gate()` Mutex pattern already used in
`tests/lua_multishard.rs` (added in v1.27.1) applied to the four
SCRIPT-cache-touching tests in `lua_eval.rs`. No production code
changes.

Independent crate version bumps:
- workspace        1.27.1 → 1.27.2
- kevy-client      1.12.12 → 1.12.13
- kevy-client-async 1.0.13 → 1.0.14
- kevy-embedded     1.4.13 → 1.4.14
- kevy-lua         1.27.1 → 1.27.2
- kevy-lua-host    1.27.1 → 1.27.2

## [v1.27.1] — 2026-06-24 (multi-shard EVAL/EVALSHA routing fix)

Bug fix discovered in real-ecosystem validation (`ioredis` against a
default 16-shard kevy):

1. **EVAL routing bug** — v1.27.0 classified EVAL as the generic
   `Route::Single(1)`, routing by the script body's hash. Under
   `--threads N` with N > 1, a `SET key v` on the key's owner shard
   followed by an `EVAL "redis.call('GET', KEYS[1])" 1 key` landed
   on a different shard and read `$-1\r\n` instead of the value.
2. **SCRIPT cache per-shard** — v1.27.0 kept the SHA1 → source
   cache inside the per-shard Bridge. `SCRIPT LOAD` arriving on
   shard X filled X's cache; a subsequent `EVALSHA` arriving on
   shard Y returned `-NOSCRIPT`.

Both bugs were silent under `--threads 1` (the default for
`cargo run -p kevy --bin kevy`), so the v1.27.0 single-Store
integration tests never surfaced them. Surfaced by the real-ecosystem
test harness in the kevy maintainer's notes.

Fixes:

- `cmd_resolve.rs::route_for_verb` now classifies
  `EVAL`/`EVALSHA`/`EVAL_RO`/`EVALSHA_RO` with `numkeys ≥ 1` as
  `Route::Single(3)` (route by KEYS[1]); `numkeys == 0` stays
  `Route::Local`. `SCRIPT` subcommands stay `Route::Local` (they
  hit the global cache).
- `cmd_lua.rs` script cache moved to a process-global
  `OnceLock<Mutex<HashMap<[u8; 20], Vec<u8>>>>`. `SCRIPT LOAD`
  writes there; `EVAL` auto-fills it; `EVALSHA` looks up source
  and calls `LuaHost::eval(source, ...)` directly (bypassing the
  per-Bridge `evalsha` whose cache is shard-local).
- The same fix applies to `cmd_lua.rs::SCRIPT EXISTS` and
  `SCRIPT FLUSH` — both operate on the global cache, no per-shard
  LuaHost touched.

Tests:

- New `crates/kevy/tests/lua_multishard.rs` — boots a real 4-shard
  kevy server in-process and verifies:
  - SET then EVAL `GET KEYS[1]` consistent across 50 keys
  - Redlock canonical unlock script consistent across 30 keys
  - SCRIPT LOAD on any shard reaches EVALSHA on any shard across
    30 different keys
  - SCRIPT FLUSH clears the global cache
- 25/25 real-ecosystem canonical script tests via `ioredis` pass
  against default-shard (16) kevy. Equivalent test failed 2/25
  under v1.27.0 same config.

Independent crate version bumps:
- workspace        1.27.0 → 1.27.1
- kevy-client      1.12.11 → 1.12.12
- kevy-client-async 1.0.12 → 1.0.13
- kevy-embedded     1.4.12 → 1.4.13
- kevy-lua         1.27.0 → 1.27.1
- kevy-lua-host    1.27.0 → 1.27.1

Still deferred to v1.28 (per the L1 "v1.27 = Lua only" lockdown):
- `cjson` / `cmsgpack` host stdlib (BullMQ / Sidekiq Pro unblock)
- `FUNCTION LOAD` / `FCALL`
- LDB debugger
- i18n `docs/lua` mirrors (ja + zh-CN)


## [v1.27.0] — 2026-06-23 (server-side Lua scripting via luna)

Lua scripting headline:

- New commands: `EVAL`, `EVALSHA`, `EVAL_RO`, `EVALSHA_RO`,
  `SCRIPT LOAD` / `EXISTS` / `FLUSH`.
- Backed by the in-house pure-Rust [luna](https://github.com/goliajp/luna)
  runtime (`luna-core 1.1`, 0-dep interpreter — the kevy 0-dep
  workspace rule is preserved; `cargo tree -p kevy-lua-host`
  shows luna-core as the only third-party crate).
- **Default Lua 5.1** (Redis ecosystem default — BullMQ / Redlock /
  rate-limiter scripts run unmodified).
- **Per-script `#!lua version=N` shebang** opts into Lua 5.2 / 5.3 /
  5.4 / 5.5. Extends Redis 7.0's `#!lua name=...` Functions syntax
  with a `version=` key. Unknown tags rejected with `-ERR unknown
  lua version`.
- Full `redis.*` host surface (call / pcall / status_reply /
  error_reply / sha1hex / log / replicate_commands).
- Read-only enforcement via `kevy::cmd::is_write_verb` — `EVAL_RO`
  rejects writes with `-READONLY can't write against a read-only
  script` (P7c).
- Cluster-mode cross-slot enforcement — when `[cluster] enabled =
  true`, multi-key EVAL whose KEYS hash to different CRC16 slots
  returns `-CROSSSLOT Keys in request don't hash to the same slot`
  (P7d).
- TOML config:

      [lua]
      time_limit_ms = 5000              # match Redis lua-time-limit
      allow_dialects = "5.1,5.3"        # comma-list; empty = all 5

  Wires to luna's `set_instr_budget` (~40 000 instr/ms) + the
  bridge's `allow_dialects` mask. Default `time_limit_ms = 5000`,
  `allow_dialects = "" (all)` (P7e).
- Two new crates added under the workspace 0-dep carved exemption
  rule:
  - `kevy-lua` — bridge (sandbox + redis.* + RESP marshaling +
    shebang + SHA1 cache).
  - `kevy-lua-host` — kevy-side glue (`LuaHost<T>` scoped-pointer
    indirection so the dispatch closure can reach `&mut Store`).

Coverage:

- 112 kevy-lua + kevy-lua-host unit/integration tests.
- 26 kevy-side end-to-end tests (`tests/lua_eval.rs` +
  `tests/lua_cluster.rs`) covering EVAL/EVALSHA/SCRIPT round-trips,
  shebang routing, read-only enforcement, cluster cross-slot, and
  the canonical Redlock + atomic-counter scripts from the v1.27
  ecosystem-survey corpus.
- SHA-1 verified against openssl + 7 standard FIPS / RFC 3174
  vectors.

Independent crate version bumps (kevy-client tracks its own minor
cadence, kevy-embedded + kevy-client-async are patch-level for the
workspace bump):

- workspace 1.26.6 → **1.27.0**
- kevy-client 1.12.10 → 1.12.11
- kevy-client-async 1.0.11 → 1.0.12
- kevy-embedded 1.4.11 → 1.4.12
- kevy-lua (new) → 1.27.0
- kevy-lua-host (new) → 1.27.0

Deferred to v1.28+:

- `cjson` / `cmsgpack` (need pure-Rust replacements — kevy 0-dep
  rule rejects C-interface ports).
- `FUNCTION LOAD` / `FCALL` (Redis 7.0 Functions surface).
- LDB-style script debugger.
- Sliding-window rate limiter — needs kevy `ZREMRANGEBYSCORE`,
  scheduled for the v1.26.x patch line.
- i18n docs/lua mirrors (ja + zh-CN).

Reference: [`docs/lua.md`](docs/lua.md).
Reference: [`.claude/rfcs/2026-06-23-v1.27-luna-bridge.md`](.claude/rfcs/2026-06-23-v1.27-luna-bridge.md) (the v1.27 phase plan).


## [v1.26.6] — 2026-06-22 (v1.26.5 follow-up — stronger crates.io 429 backoff)

v1.26.5's publish chain made it to crate #9 (kevy-map) before
crates.io 429'd; the 65 s × 3 retry wasn't long enough to outwait
the burst window after 16+ publishes had already happened that hour.

Workflow `publish_or_skip` now:
- sleeps 35 s after every successful publish (instead of 3 s) — stays
  well under any plausible per-10-minute publish limit even on a
  clean chain
- on 429: 300 s × up to 5 retries (was 65 s × 3) — max 25 min wait
  per crate

No source change. Worst-case 22-crate chain: ~13 min happy path,
~38 min if the first publish trips a depleted window.

8 / 22 crates are at 1.26.5 on crates.io from v1.26.5's partial
chain; v1.26.6 republishes all 22 fresh.

- workspace 1.26.5 → 1.26.6
- kevy-client 1.12.9 → 1.12.10
- kevy-client-async 1.0.10 → 1.0.11
- kevy-embedded 1.4.10 → 1.4.11

## [v1.26.5] — 2026-06-22 (v1.26.4 follow-up — aarch64-linux prefetch cfg-guard + crates.io rate-limit retry)

v1.26.4 fixed the unlink/chmod FFI signature but the
`aarch64-unknown-linux-gnu` binary build still failed because
`crates/kevy-rt/src/uring_arm.rs:139` uses `core::arch::x86_64::_mm_prefetch`
unconditionally. x86_64-only intrinsic; gate it behind
`#[cfg(target_arch = "x86_64")]`. The hardware prefetcher handles
the cold-cache hint on non-x86_64.

Also: v1.26.4's publish chain tripped crates.io's "30 updates/min"
rate limit at kevy-elect (the v1.26.2 + v1.26.3 + v1.26.4 tag
sequence published >40 crate versions in a few minutes). Add to
the workflow's `publish_or_skip`:
- 3 s sleep between successful publishes (~5/min cap = safe)
- on `429 Too Many Requests`, sleep 65 s + retry up to 3×

No source change beyond the cfg-guard. Workflow only.

- workspace 1.26.4 → 1.26.5
- kevy-client 1.12.8 → 1.12.9
- kevy-client-async 1.0.9 → 1.0.10
- kevy-embedded 1.4.9 → 1.4.10

## [v1.26.4] — 2026-06-22 (v1.26.3 follow-up — aarch64-linux build fix)

v1.26.3 published all 16 crates to crates.io successfully (the
v1.25 → v1.26.3 publish chain is complete), but the `aarch64-linux`
binary build for the GitHub Release archive failed with E0308: the
v1.25 UDS FFI declared `pub fn unlink(path: *const i8)` /
`chmod(path: *const i8, ...)`. That compiles on x86_64-linux and
aarch64-apple-darwin where `c_char = i8`, but on aarch64-linux
`c_char = u8` and the `CStr::as_ptr() -> *const c_char` callsite
mismatches.

Fix: switch the two FFI signatures to `*const core::ffi::c_char`,
which resolves to the right primitive on every target.

- workspace 1.26.3 → 1.26.4
- kevy-client 1.12.7 → 1.12.8
- kevy-client-async 1.0.8 → 1.0.9
- kevy-embedded 1.4.8 → 1.4.9

## [v1.26.3] — 2026-06-22 (v1.26.2 follow-up — kevy-resp manifest fix)

`cargo publish` failed in v1.26.2 because `crates/kevy-resp/Cargo.toml`
declared its `kevy-bytes` dependency with `path = "../kevy-bytes"`
but no `version = "..."` floor (added in commit 47cd0eb on 2026-06-20
along with the SIMD `find_crlf` work — local builds didn't notice
because path-deps resolve fine, and v1.22.0 was the last successful
publish so no later workspace publish exercised the manifest).

Fix: pin `kevy-bytes = { path = "../kevy-bytes", version = "1.19.0" }`
matching the pattern in `kevy-store/Cargo.toml`.

No source change.

- workspace 1.26.2 → 1.26.3
- kevy-client 1.12.6 → 1.12.7
- kevy-client-async 1.0.7 → 1.0.8
- kevy-embedded 1.4.7 → 1.4.8

## [v1.26.2] — 2026-06-22 (v1.26.1 follow-up — lx64 runner CARGO_HOME override)

v1.26.1 failed verify on lx64 because the runner runs as `gha-runner`
(not root), whose `$HOME/.cargo/bin` was empty and whose systemd
unit sets a root-owned `CARGO_HOME=/mnt/ssd980/cargo-cache` for
shared build caching. Two fixes:

- Server-side: install rustup + stable toolchain into
  `/home/gha-runner/.cargo` (one-time, no workflow change).
- Workflow-side: also export `CARGO_HOME` and `RUSTUP_HOME` to the
  gha-runner's home for the verify job, so cargo's package cache is
  writable.

No code change.

- workspace 1.26.1 → 1.26.2
- kevy-client 1.12.5 → 1.12.6
- kevy-client-async 1.0.6 → 1.0.7
- kevy-embedded 1.4.6 → 1.4.7

## [v1.26.1] — 2026-06-22 (v1.26.0 follow-up — rustup PATH on lx64 runner)

v1.26.0 verify ran on the self-hosted lx64 runner as intended but
failed because the actions step shell didn't inherit `$HOME/.cargo/bin`
in `$PATH` — `rustup` lives there but the step couldn't find it.
Fix: prepend `$HOME/.cargo/bin` to `GITHUB_PATH` as the first step
of every job that touches Rust.

No code change; only `.github/workflows/release.yml` and version bumps.

- workspace 1.26.0 → 1.26.1
- kevy-client 1.12.4 → 1.12.5
- kevy-client-async 1.0.5 → 1.0.6
- kevy-embedded 1.4.5 → 1.4.6

## [v1.26.0] — 2026-06-22 (v1.25 redo — docs sweep + self-hosted CI runner)

Re-ship of v1.25.0. The v1.25.0 tag was pushed but the Release
workflow failed at the `Verify tag builds` step on a free
GitHub-hosted runner (8-shard `blocking_cross_shard::blpop_remote_key_immediate_hit`
test allocates more memory than the 2-vCPU / 7-GB runner has after
the v1.25 K1/K2 PBUF + io_uring ring bump). v1.25.0 therefore never
reached crates.io / GH Releases. Two changes vs the v1.25.0 tag:

- `.github/workflows/release.yml` — `verify` job switched from
  `ubuntu-latest` to `[self-hosted, lx64]` (the org-registered lx64
  bare-metal runner the perf work already runs on; 16 cores / 64 GB
  RAM, no ENOMEM).
- Comprehensive doc sweep landing on top: README.md (en/ja/zh),
  bench/REPORT.md, crates/kevy-embedded/README.md, kevy-sys + kevy
  READMEs, docs/tuning.md (en/ja/zh), and a new docs/uds.md (en/ja/zh)
  covering precision-bench numbers + embed-server联合 deployment
  shapes — see commit `25e074b`.

Code-side `kevy-*` crates are byte-identical to the v1.25.0 build;
only Cargo.toml `version` fields and three crate-local versions move:

- workspace `1.25.0 → 1.26.0`
- `kevy-client 1.12.3 → 1.12.4`
- `kevy-client-async 1.0.4 → 1.0.5`
- `kevy-embedded 1.4.4 → 1.4.5`

Everything below remains true (it's the v1.25.0 entry, unchanged).

## [v1.25.0] — 2026-06-22 (decomposition-driven perf sprint + UDS support)

This release adopts and ships the
**decomposition-then-attack methodology** (`.claude/rule/perf-vs-foss.md`,
adapted from the SPG project's `PERF_METHODOLOGY_VS_FOSS.md`). Every
v1.25 attack started from a per-axis Phase A decomposition
(`.claude/notes/v125-deco-axis-*.md`) that enumerated 18+ stages of
the kevy and valkey paths side-by-side, file:line × atomic-op-count,
total ±20 % of measured wire RTT. Phase B attacks then implemented
the Top-N attack list from each decomposition.

**8 of 11 pre-v1.25 axis hypotheses were refuted** by Phase A reading.
The full outcomes report is `bench/V125-AXES-MASTER.md`.

v1.25.0 supersedes v1.24.1 UNRELEASED — both ship together (v1.24.1's
12-attack chain remains, listed below; the new v1.25 attacks are
additive on top).

### v1.25 — Shipped (Phase B attacks)

| Group | Commit   | Axis | Attack                                                                                                | Result                                                                |
|-------|----------|------|-------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------|
| G1    | 01948ca  | K    | `PBUF_ENTRIES` 128 → 4096 + `URING_ENTRIES` 256 → 2048 (kevy-rt/uring_reactor.rs)                     | c=10 000 t=1 SET **270 → 120 178 rps (+44 511×)**; cliff resolved     |
| G2    | f763146  | I+B  | Parse-from-slab fast path + big-arg pre-grow + epoll output_arcs correctness fix                       | Axis I GET p999 0.527 → 0.407 ms (-23 % vs valkey); B 64K SET 95→103 % |
| G3    | 9d2c03f  | C    | Hoist `maxmemory>0` gate (F3) + canonical-i64 first-byte guard (F2')                                  | Cross-axis -10-15 ns/SET; below variance band at c=50 -P 1            |
| H1.A  | 4b72ec0  | H    | pub/sub `nshards==1` fast path                                                                        | Component of G5 chain                                                  |
| G5    | 6587032  | H    | per-channel `subs_by_channel` index + `pending_write` dedup + Arc-shared message body + writev gather | subs=50 **452 %** vs valkey; subs=500 **517 %**; subs=10 flipped to WIN vs redis |
| G4    | 4ec1278  | G    | Borrowed-slice dispatch for SADD/SREM/HSET/HMGET/HDEL/LPUSH/RPUSH/ZADD/ZREM/DEL/EXISTS                | Kills N+1 mallocs per multi-arg cmd; structural, +1 % bench           |

### v1.25 — Reverted with measurement (R3 ★)

Two attacks from `.claude/notes/v125-deco-axis-i-c50-10kb.md` did NOT
match their Phase A predictions; reverted after bench:

- **G6 A2 lazy-drop big values** — predicted -20-150 µs p999;
  measured **+144 µs** (worse). Single-thread deferred bunching
  produces periodic batched stalls bigger than the inline drops it
  replaced. valkey's `lazyfree.c` wins via a separate bio thread,
  not the deferral itself.
- **G6 A4 `submit_and_wait(1)` only-writes** — predicted -50-200 µs
  p999; measured **+44 % p999**. The spin ladder existed precisely
  so burst arrival catches the next recv within the spin window.

Both negative results are captured in `bench/V125-AXIS-I-LATENCY.md`
and `bench/V125-AXES-MASTER.md` as R3 ★ flipped predictions.

### v1.25 — Deferred (named cause + fix path, not ceiling claims)

| Item                                          | Blocker / fix path                                                          | Estimated gain |
|----------------------------------------------|-----------------------------------------------------------------------------|----------------|
| Axis I SET p999 / max at -d 10240             | A3 take-into-Arc on SET path; needs argv ownership in kevy-resp             | match valkey 0.335 ms p999 |
| Axis H size=4 KB pub/sub                     | writev-chunking for IOV_MAX=1024 cap                                        | ≥ 120 % vs valkey |
| Axis D single-probe `live_entry`              | `kevy-map` raw-entry API                                                    | -15-20 ns/GET |
| 64 KB GET / recv-into-Arc for big bulks       | B-A2 io_uring reactor change                                                | -6-8 µs / 64 K SET |
| `lazyfree` deferred drop                      | Bio thread for free-work                                                    | unblocks Axis I tail attack |

### v1.25 — Trigger-word ban applied to bench docs

11 bench docs (`bench/V125-AXIS-*.md` + `V125-AXES-MASTER.md`)
rewritten in commit `dcaeadc`: removed "tied / kernel-bound / loopback
floor / valkey absorbed / structural ceiling / RTT-bound hides X"
claims, replaced with file:line + atomic-op-count + named fix paths
per `.claude/rule/perf-vs-foss.md` R2.

### v1.25 — Methodology rule + memory artifacts

- `.claude/rule/perf-vs-foss.md` — R1-R8 codified rules for kevy +
  any future vs-FOSS project. CLAUDE.md project link added.
- Auto-memory entry `feedback-perf-vs-foss-decomposition` records
  the methodology + my own pre-adoption mistakes (V125-AXIS-* dev
  trail) as negative-learning case studies.

---

## [v1.24.1] — UNRELEASED, superseded by v1.25.0 (autorun perf sprint on top of v1.24.0)

User-authorized **autorun** continuation of the v1.23 → v1.24 perf
sprint, layered on top of E13 (THP-aligned mmap, v1.24.0). 11 perf
attacks shipped, 3 retired-with-rationale, 3 audit-closed, 1 deferred.

**Headline measurement (lx64, kernel 6.12, mitigations=off, io_uring):**

| Workload                  | v1.24.0 (E13 alone) | v1.24.1 (this sprint) | Δ      |
|---------------------------|---------------------|-----------------------|--------|
| Rust c1 SET               | ~76 k               | **82–84 k**           | +8–11% |
| Rust c1 GET               | ~77 k               | **83–84 k**           | +8–9%  |
| C `redis-bench` c1 SET    | ~82 k               | ~82–84 k              | matches |
| C `redis-bench` c1 GET    | ~82 k               | ~82–84 k              | matches |
| c100 SET (4-core load)    | ~150 k              | **184–188 k**         | +25%   |
| c100 GET (4-core load)    | ~140 k              | **187–191 k**         | +35%   |

The Rust client at -c1 has reached **C-client parity** — the prior gap
was userspace-side; this sprint closed it. Post-sprint H-redo
diagnostic (`bench/PERF-PROFILE-2026-06-20-POST-V124-CHAIN.md`)
confirms 38 % of remaining CPU is kernel-side (tcp_sendmsg + io_uring
admin + 1.26 % nft_do_chain loopback netfilter).

### Shipped (12 attacks + 1 diag doc)

| ID  | Where                              | Win                                          |
|-----|------------------------------------|----------------------------------------------|
| E14 | kevy-uring `submit_and_wait`       | threshold-based `io_uring_enter` skip (replaces dropped E3) |
| A2  | kevy-rt `Shard`                    | `#[repr(align(64))]` CachePadded on `inbound_dirty` + `parked` |
| A3  | kevy-rt `uring_arm_conns`          | `_mm_prefetch::<T0>` next `UringConn` ahead of loop body |
| A9  | kevy-rt `exec_dispatch`            | `#[cold]` hint on SLOWLOG-ON / AOF-ON branches |
| A5  | kevy-resp `ArgvBorrowed`           | InlineRanges (4 inline + heap spill) — pure safe Rust, no malloc on ≤4-arg cmds |
| A6+A7 | kevy-bytes new `find_crlf`       | SIMD scanner: x86_64 AVX2 (runtime-detected) + aarch64 NEON + SWAR fallback |
| C6  | kevy-config, kevy-persist          | `#[cold]` on 3 startup/AOF-rewrite-only fns (15.9 + 11.1 + 9.2 KiB) |
| B4  | kevy-uring, kevy-rt                | `IORING_ACCEPT_MULTISHOT` (Linux 5.19+) + per-CQE F_MORE re-arm gate |
| A4  | kevy-rt `Conn`                     | `#[repr(C)]` + hot-first field layout → 2 cache lines (vs 4) of hot state |
| E15 | kevy-rt `drain_inbound`            | fast-path inline + cold-body outline (`drain_inbound_core_slow`) |
| E16 | kevy-rt `flush_wakes` / `flush_backlog` | same fast-path inline + cold outline pattern |
| A13 | kevy-store `tick_expire`           | skip sampling loop when `expires == 0` (TTL-free workloads) |
| —   | bench docs                         | `PERF-PROFILE-2026-06-20-POST-V124-CHAIN.md` re-diagnosis after the chain |

### Retired with rationale (kept in code as inline notes)

- **A11** `IORING_SETUP_TASKRUN_FLAG` — 30 % c1 GET regression + multi-second 3.6k-rps stalls; bit's set/clear timing under COOP_TASKRUN doesn't align with busy-poll closely enough. Rationale block in `ring.rs::submit_and_wait`.
- **E17** outline pattern for `flush_requests` / `flush_publish` — body small enough that LLVM was already inlining; forced outline added a fn call on the cross-shard hot path with no upside. lx64 c100 SET/GET -3-8 % vs E16.
- **E18** `uring_reap_closed` fast-path bail — needed two `any()` scans (io.closing + conn.closing) because `is_quit` sets `conn.closing` only; at c100 the 2×100-iter pre-scan × 62 k reaps/s cost more than the avoided Vec::collect saved (lx64 c100 SET -2.9 %). Single-scan version requires plumbing io map into dispatch QUIT path.

### Audit-closed (no code change)

- **A8** Conn/UringConn slab — KevyMap slot array already IS the slab; Conn inline storage means no per-conn malloc.
- **B1** generalise E13 to all KevyMap users — already covered by `KevyMap::alloc_table` (the only allocation path); every map >1 MiB auto-uses 2 MiB mmap.
- **B2** std HashMap → KevyMap audit — 49 std HashMap usages all in cold control planes (kevy-elect, kevy-cluster-rw, per-cmd builders); none on -c1 hot path.

### Deferred (rationale documented per item in task tracker)

All remaining backlog items reviewed and triaged for this sprint:

- **A1** split run_uring — readability refactor; LLVM already inlines all
  helpers into one symbol so split without `#[inline(never)]` doesn't
  separate perf attribution, and with `#[inline(never)]` it costs (E17
  pattern). Pure-readability win at non-trivial bug risk.
- **A10** adaptive URING_SPIN_LIMIT — fixed 256 works across all
  measured workloads; no signal it's wrong.
- **A12** linked SQE write→close — restructures Socket Drop + fd
  transfer for 1 saved libc::close per conn; 0 closes at -c1.
- **A14** PubSub RCU — 0-dep lock-free Arc swap needs hazard pointers
  or epoch GC (multi-hundred LOC unsafe); no PubSub workload in bench.
- **B3** per-shard arena — malloc bucket already 0.50% post-A5+E13.
- **B5 / E5** MSG_ZEROCOPY (send + recv) — two-CQE flow only wins on
  > 4 KB payloads; redis-benchmark replies are < 20 bytes.
- **B6** REGISTER_BUFFERS — needs fixed-size Conn output buffers (kevy
  grows Vec per reply); restructure breaks unbounded reply pattern.
- **B7** RESP3 push-frame default-on — **NOT DOING** (RESP2 wire compat
  lock).
- **C1** BOLT — needs llvm-bolt installed on lx64 (host write).
- **C2** AutoFDO — needs LLVM create_llvm_prof + rustc nightly profile-
  sample-use; CI pipeline scope.
- **C3** hot-section linker — needs custom ld script + RUSTFLAGS infra;
  real win requires D1 hugetlbfs to land alongside.
- **C4** strip panic strings — `-Z fmt-debug=none` is nightly-only.
- **C5** musl static link — `x86_64-unknown-linux-musl` target not
  installed on either build host.
- **C7** mold linker — not installed on lx64.
- **C8** PGO in CI — depends on C2 landing first.
- **D1–D9** all host-side (kernel boot params, irqaffinity, cpupower,
  SMT toggle, SCHED_FIFO, TCP sysctls, custom kernel, iptables fast-
  path). Shared-box no-touch policy.
- **E6** shared-mem transport — new wire + client + multi-process shm
  mgmt; multi-week scope.
- **F3** LZ4 compression — storage/memory feature; pure-Rust 0-dep
  impl is ~1k LOC fuzz-clean.
- **F4** QUIC/HTTP3, **F5** gRPC — **NOT DOING** (RESP wire compat
  lock).
- **G2** NUMA routing, **G3** topology-aware client — lx64 is single-
  socket; no NUMA topology to exploit.

### Public API

No new public API additions. SIMD `find_crlf` is `pub` in kevy-bytes
(new entry point) but otherwise internal. kevy-uring adds
`prep_accept_multishot` (pub).

### Wire / persistence

No changes.

### Version bumps (for the release machinery to apply on tag)

- workspace `1.24.0` → `1.24.1` (perf-only, no API break)
- `kevy-bytes`: new `pub fn find_crlf` is additive minor (could stay
  patch since it's an addition not a break)
- All other crates: patch bump for chain rebuild only

---

## [v1.24.0] — 2026-06-20 (E13 — 2 MiB-aligned mmap path for kevy-map THP)

After v1.23.2 closed the "incremental perf" sprint, user authorized
architectural work as long as API + project principles hold. E13 is
that architectural win:

### Why

`kevy-map`'s hash table called `kevy_madvise::advise_hugepage()` on its
allocation. The advise was correct, but the global allocator
(jemalloc-like chunk placement) returned a 4 KiB-aligned base pointer
inside a larger arena. `khugepaged` cannot find a 2 MiB-aligned
candidate to promote inside a 4 KiB-aligned arena. Observed
empirically: `AnonHugePages: 0 kB` in `/proc/PID/smaps` despite the
hint, for the entire v1.23.x line.

### Fix

`kevy-madvise` gains two new entry points:

- **`mmap_anon_aligned_2mb(len)`** → `Option<NonNull<u8>>` — anonymous
  `mmap` with a 2 MiB-aligned base + 2 MiB-multiple length + immediate
  `MADV_HUGEPAGE`. Implements the classic "over-allocate by one HP,
  trim prefix/suffix via munmap" alignment trick. Linux only;
  `None` on other targets so the caller falls back to the global
  allocator.

- **`munmap_2mb(ptr, len)`** → matching cleanup, rounds `len` up
  internally to match the allocation.

`kevy-map`'s `alloc_table` (extracted into a new `alloc.rs` for the
500-LOC house rule) picks between the two paths at a 1 MiB threshold:
small tables stay on the global allocator (over-allocation cost not
worth it); large tables go through the mmap path and finally get
THP-aligned storage. A new `pub(crate)` `mmap_backed: bool` field
tracks which dealloc to call in `Drop`.

### Measured

On the lx64 reference (Intel i7-10700K Comet Lake, Linux 6.12,
mitigations=off):

- `/proc/PID/smaps` `AnonHugePages`: **0 kB → 40 960 kB** (20 × 2 MiB
  pages promoted after a 2 M-key SET workload)
- C `redis-benchmark` c1 SET: ~80 k → ~82.8 k (+3%)
- C `redis-benchmark` c1 GET: ~80 k → ~81.9 k (+2%)

The throughput delta is small at -c1 because that workload is syscall-
bound, not memory-bound. The architectural win is the THP mechanism
finally working as designed. Memory-bound workloads (large keyspaces
under -c50 -P16) should see proportionally more.

### Public API

- `kevy-madvise`: **two new pub fns added** (`mmap_anon_aligned_2mb`,
  `munmap_2mb`); existing `advise_hugepage` unchanged. Minor bump.
- `kevy-map`: no public API change; the new `mmap_backed` field is
  `pub(crate)` and internal.

### Version bumps

- workspace `1.23.2` → `1.24.0` (kevy-madvise API additive — minor)
- `kevy-client` `1.12.2` → `1.12.3` (dep rev only)
- `kevy-embedded` `1.4.3` → `1.4.4` (dep rev only)
- `kevy-client-async` `1.0.3` → `1.0.4` (dep rev only)

### Wire / persistence / API

No changes to the RESP wire protocol, AOF/snapshot format, CLI flags,
or kevy-map's public surface. kevy-madvise gains two additive pub fns.

---

## [v1.23.2] — 2026-06-20 (perf sprint closeout — E12 + final diagnostic)

Final patch in the v1.23.x perf sprint. After v1.23.1 user asked
me to keep going until I genuinely ran out of incremental directions.
Two more attacks (#22 + #23 in the cumulative log):

### Code changes

- **E12** (`kevy-rt`) — `std::hint::spin_loop()` in the io_uring
  reactor's idle busy-poll branch. Compiles to PAUSE on x86 /
  YIELD on ARM. Industry-standard idiom for busy-loops in Rust
  1.49+ stable:
  - Lower power draw on a quiet shard
  - Frees pipeline bandwidth for the SMT sibling
  - Reduces branch-history pollution from speculative reads

  Throughput at single-conn bench is in noise (Rust c1 ~76k, C c1
  ~80k); benefit shows up on multi-shard / SMT configurations.
  Zero regression risk.

### Diagnostic / documentation

- **Attack 22** — `perf stat -e dTLB-loads,iTLB-loads,L1-*`. Found
  data TLB is fine (0.00% miss) but **iTLB is over-saturated
  (228% miss ratio)** at -c1. Also revealed that THP isn't landing
  on the kevy-map main allocation despite the `advise_hugepage()`
  call, because the global allocator's base pointer isn't
  2 MB-aligned. Both findings logged for future work:
  - iTLB pressure mitigation needs code-size reduction or a code-
    segment hugetlb deployment recipe
  - kevy-map THP landing needs a custom 2 MB-aligned `mmap`-based
    allocator OR a `hugetlbfs` deployment recipe

  Real engineering tasks; **deferred** beyond this incremental
  sprint.

- **PERF-ATTACK-LOG-2026-06-20.md** — attacks 22 + 23 logged. Final
  scoreboard:

  **23 attacks total. 16 kept (14 code + 2 doc), 4 dropped,
  3 diagnostic-only.**

### Cumulative status

Numbers unchanged from v1.23.1 (E12 throughput delta is in noise at
single-conn bench; gain shows up on multi-shard layouts).

Default-friendly config (mitigations=off, ruleset on):
- C c1 GET: 68 k (v1.22) → **84.9 k** (+25%)
- C c1 SET: 76 k (v1.22) → **84.9 k** (+12%)
- vs valkey-iot lead: **1.23-1.33×**

Fully tuned (mitigations=off + nft flush + PGO):
- C c1: **~108 k** SET/GET; **1.57-1.69×** valkey-iot

### Version bumps

- workspace `1.23.1` → `1.23.2`
- `kevy-client` `1.12.1` → `1.12.2` (dep rev only)
- `kevy-embedded` `1.4.2` → `1.4.3` (dep rev only)
- `kevy-client-async` `1.0.2` → `1.0.3` (dep rev only)

### Wire / persistence / API

No changes.

---

## [v1.23.1] — 2026-06-20 (perf sprint extension — branch-prediction + host knobs)

A follow-on patch release after v1.23.0. User pushed back on
declaring perf "done" too early; this round added 5 more attacks
(17–21 in the cumulative log) including the biggest single
host-tuning lever found in the entire sprint.

### Code changes

- **E11** (`kevy-rt`) — reorder the per-completion match dispatch
  in `Shard::run_uring`: hot arms `OP_RECV` / `OP_WRITE` come
  first, cold arms `OP_ACCEPT` / `OP_WAKER` / `OP_TIMEOUT` call a
  `#[cold] #[inline(never)]` no-op marker fn to flip LLVM's
  branch-predictor hint. Diagnostic that drove the attack: switched
  perf event from `cycles` to `branch-misses` and found
  `Runtime::run::closure` was **33.22%** of all branch
  mispredictions across kevy.
  - Closure share of branch-misses: 33.22% → **3.68%** (-89%)
  - IPC: 1.63 → 1.70 (+4%)
  - C c1 SET: ~80k → ~83k (+4%)
  - C c1 GET: ~75k → ~81k (+8%)

### Documentation

- **E6** — `docs/tuning.md` (en/ja/zh-CN): added a major section on
  emptying the netfilter / iptables ruleset. Measured **25-35%
  throughput jump** on the lx64 reference (C c1 SET 80.6k → 108.9k);
  the biggest single host-tuning lever found in the sprint. Trade-off
  documented in full (breaks docker port forwarding, libvirt NAT,
  firewall posture). Safer half-measure (`iptables -I INPUT 1 -p
  tcp --dport 6004 -j ACCEPT`) recovers ~half the gain while keeping
  the firewall intact.

- **PGO recipe** — `docs/tuning.md` (en/ja/zh-CN): step-by-step
  PGO recipe for fixed-workload deployments. Measured 1-10% on the
  lx64 reference; workload-bound so NOT shipped in CI default.

- **PERF-ATTACK-LOG-2026-06-20.md** — updated with attacks 17-21.
  21 attacks total in the cumulative sprint: 14 kept (12 code + 2
  doc), 4 dropped, 5 doc-only / diagnostic.

### Cumulative status (post-v1.23.1)

Default-friendly config (mitigations=off but ruleset on):
- C `redis-benchmark` c1 GET: 68 k (v1.22) → **84.9 k** (+25%)
- C `redis-benchmark` c1 SET: 76 k (v1.22) → **84.9 k** (+12%)
- vs valkey-iot c1 lead: 1.13× → **1.23-1.33×**

Fully tuned (mitigations=off + nft flush + PGO):
- C c1: **~108 k** SET/GET — the true server ceiling on this
  hardware; **1.57-1.69×** valkey-iot

### Version bumps

- workspace `1.23.0` → `1.23.1`
- `kevy-client` `1.12.0` → `1.12.1` (dep rev only)
- `kevy-embedded` `1.4.1` → `1.4.2` (dep rev only)
- `kevy-client-async` `1.0.1` → `1.0.2` (dep rev only)

### Wire / persistence / API

No changes. Same RESP wire protocol, same AOF/snapshot format,
same CLI flags, same public Rust API surface.

---

## [v1.23.0] — 2026-06-20 (profile-driven perf sprint, 16 attacks)

A profile-driven perf sprint on top of v1.22.0. Headline numbers on the lx64
reference (Intel Xeon 6, Linux 6.12, 10 shards on 16 cores):

| Workload (io_uring reactor, mitigations=off) | v1.22.0  | v1.23.0  | Δ     |
|----------------------------------------------|----------|----------|-------|
| C `redis-benchmark` -c1 GET                  | 68 k     | **84 k** | +24%  |
| C `redis-benchmark` -c1 SET                  | 76 k     | **84 k** | +11%  |
| Rust client -c1 GET                          | 59 k     | ~75 k    | +27%  |
| Rust client -c1 SET                          | 59 k     | ~73 k    | +24%  |

vs valkey 9.1 (io-threads, same host):
- -c1 GET: 84 k vs 69 k = **1.22×** (was 1.13×)
- -c1 SET: 84 k vs 64 k = **1.31×** (was 1.27×)

The -c50 -P16 numbers (6 M/s GET, 4 M/s SET) hit the `redis-benchmark`
client-side cap with `--threads 6`; the server has more headroom but the
test harness can't push faster.

Sprint methodology: top-down `perf record` flamegraph on the lx64 reference
(documented in [`bench/PERF-PROFILE-2026-06-20.md`](bench/PERF-PROFILE-2026-06-20.md)).
Each attack measured before and after; verdicts + per-attack measurement
in [`bench/PERF-ATTACK-LOG-2026-06-20.md`](bench/PERF-ATTACK-LOG-2026-06-20.md).
**16 attacks** total: 12 kept, 4 dropped.

### Reactor open-loop wins

- **D1** — `inbound_dirty` u64 bitmap (`kevy-rt`): replaces N-shards
  `drain_inbound` sweep with single `AtomicU64::swap` on a dirty bitmap.
  `drain_inbound` self-time 17.4% → 7.2% of -c1 CPU.
- **D2** — `pending_wakes` + `backlog_nonempty` u64: same bitmap shape
  for cross-shard wake + backlog short-circuits.
- **D3** — `request_batch` + `publish_batch` u64: same bitmap shape for
  cross-shard request/publish flush.
- **E8** — `Acquire`-load fast path on `inbound_dirty`: cheap `mov` on
  x86 TSO instead of `lock xchg` per reactor iter when no peer has
  marked us. `drain_inbound` 4.86% → 2.90%.
- **E9** — hoist replication-pump gate to call site so the standalone
  shard pays one branch instead of two function-call frames per iter.
  `pump_replication` + `reap_closed_replicas` 2.04% → 0 from top 15.

### io_uring kernel-side wins

- **E1.5** — `IORING_REGISTER_RING_FDS` (`kevy-uring`): self-register the
  ring's fd into the per-thread registered-rings table; `io_uring_enter`
  references it by index instead of raw fd. Kernel skips `fget`+`fput`
  per syscall. **8 pp kernel cost eliminated**; C c1 SET +6.4% (in
  isolation).
- **E2** — `IORING_SETUP_SINGLE_ISSUER | COOP_TASKRUN`: modern setup
  flags (Linux 6.0+ / 5.19+). Kernel skips submission-side locking +
  waits for natural enter instead of IPI. +3–5% Rust c1.
- **E4** — kernel `mitigations=off` (deployment): the lx64 reference
  rebooted with `mitigations=off`; `clear_bhb_loop` (Spectre BHB)
  eliminated from the syscall path. Single biggest lever in the sprint:
  +12% on C c1 SET, +20% on c1 GET, +24% on c50-P16. Documented as a
  trade-off in `docs/tuning.md`; **only for trusted single-tenant boxes**.
  See the doc for the security implications.

### Client surface

- **D4** — `kevy_resp::encode_command_borrowed` + new
  `kevy_resp_client::Connection::request_borrowed(&[&[u8]])` zero-alloc
  request path. 20+ `kevy_client::Connection` methods now reuse a
  pooled `write_buf`. `kevy-client` bumped to **1.12.0** (additive).

### Documentation / inlining

- **D6** — [`docs/tuning.md`](docs/tuning.md) + ja/zh-CN translations:
  CPU pinning, AOF off for replicas, `KEVY_IO_URING=1`, kernel
  `mitigations=off` (with full security trade-off discussion).
- **E7** — `#[inline]` hints on RESP parser hot helpers
  (`parse_command_borrowed`, `parse_bulk_len`, `find_crlf`, `parse_int`).
- **E10** — `#[inline]` on remaining reactor flush/drain helpers
  (`flush_wakes`, `uring_drain_inbound`, `drain_inbound_core`).

### Investigated, NOT shipped

- **D5** + **E5** — `io_uring` SQPOLL (attempted twice). Wire-level
  `IoUring::new_sqpoll` ships in `kevy-uring` but is **not wired into
  kevy-rt's shard reactor**. SQPOLL spawns one kernel poll thread per
  ring; in kevy's shared-nothing thread-per-core layout this either
  fights the shard threads for cores (D5 measured 2–15× regression),
  or — with disjoint affinity (E5) — adds cross-core synchronization
  per SQE that exceeds the saved syscall (E5 measured 2–29% regression).
- **E1** — `IORING_REGISTER_FILES_SPARSE` + `IOSQE_FIXED_FILE` per-conn
  registered files. Wire-level API ships in `kevy-uring` but **not
  wired into kevy-rt**. The visible `fget` in kevy's profile is the
  ring-fd lookup in `__do_sys_io_uring_enter`, not per-SQE fd lookup;
  IOSQE_FIXED_FILE wasn't on the right path. E1.5's
  `IORING_REGISTER_RING_FDS` is the lever that attacked the visible cost.
- **E3** — skip `io_uring_enter` on `to_submit == 0 && wait_nr == 0`.
  Regressed 16–25% because E2's `COOP_TASKRUN` flag flips the
  kernel-userland cooperative contract — kernel waits for the user task
  to enter naturally to run task_work; skipping starves completion
  processing.

### Version bumps

- workspace `1.22.0` → `1.23.0`
- `kevy-client` `1.11.0` → `1.12.0` (D4 additive API)
- `kevy-embedded` `1.4.0` → `1.4.1` (dep rev only)
- `kevy-client-async` `1.0.0` → `1.0.1` (dep rev only)

### Wire / persistence / API

No changes. Same RESP wire protocol, same AOF/snapshot format, same CLI
flags, same public Rust API surface (D4 is additive).

---

## [v1.22.0] — 2026-06-20 (v3-cluster close — Phase 2 + Phase 3 + Phase 4)

Bundle release closing v3-cluster: **embed-as-read-replica**
(Phase 2), **scoped multi-writer** (Phase 3), and **async client**
(Phase 4). Three phases shipped together as one coherent v3
upgrade per user policy. Server / persistence / pub-sub paths are
unchanged from v1.19; this release lands new surface across
`kevy-embedded`, `kevy-client`, the new `kevy-scope` and
`kevy-client-async` crates, plus the cluster cement in `kevy/`
and topology refinements in `kevy-cluster-rw` / `kevy-elect`.

---

### Phase 2 — embed-as-read-replica

An application embedding `kevy-embedded` can mirror a server
primary's keyspace in-process — reads pay zero network round-trip;
local writes return `READONLY` (the replication stream is the only
writer). Same `kevy_replicate::ReplicaClient` wire client that
drives v1.18 server replicas drives the embed runner.

- **`kevy_embedded::Store::open_replica(upstream)`** — convenience
  constructor (`without_aof` + upstream + default reconnect
  100 ms → 5 s). Returns a normal `Store` with
  `is_replica() == true`; cloneable and droppable like any other.
- **`Config::with_replica_upstream(host:port)`,
  `with_replica_id(id)`, `with_replica_reconnect(min, max)`** —
  full builder control. Default replica id is
  `kevy-embedded-replica`; override per process when multiple
  replicas share one primary.
- **`Store::is_replica()`** — live query of replica mode.
- **READONLY enforcement** — every mutating embed API
  (`set` / `del` / `incr_by` / `expire` / `flushall` / `hset` /
  `hdel` / `lpush` / `rpush` / `lpop` / `rpop` / `sadd` / `srem` /
  `zadd` / `zrem` / `persist`) returns
  `io::Error::other("READONLY ...")` on a replica. Wire string
  mirrors the server-side `-READONLY` reply so applications
  pattern-match the same way against both backends. `PUBLISH`
  remains allowed (pub/sub is process-local).
- **`kevy_embedded::replica_runner` (pub(crate))** — one
  background thread per `Store::open_replica`, drives a real
  `kevy_replicate::ReplicaClient`. Exponential reconnect
  (sliceable so shutdown is acted on within `backoff_min`),
  interruptible `next_event`, joined on last `Store` clone drop
  via `DropGuard`.
- **`docs/cluster.md` "embed-as-read-replica" section** + runnable
  example `crates/kevy-embedded/examples/replica.rs`.

Internals: new `replica_glue.rs` (`spawn_replica_runner`,
`ensure_writable`), extracted `store_persist.rs` to keep
`store.rs` under the 500-LOC project ceiling.

Anti-scope contracts: single upstream URL = single primary shard
mirror (multi-shard upstream is "spawn N replicas" for v1.22). No
snapshot ingest (a replica connecting at offset 0 against a
primary whose backlog has rolled past drops the connection — full
ingest is a follow-up). No auto-retarget on `kevy-elect`
ANNOUNCE; pair with `kevy-cluster-rw` topology refresh for the
automated path. No replica writes — `READONLY` is the contract.

---

### Phase 3 — scoped multi-writer

Per-prefix writer ownership with optional server-backed fallback,
longest-prefix routing, `-MISDIRECTED writer is <host:port>`
redirect, and `MOVE-SCOPE` quiesce-window migration
(Q3 = (a) per RFC). Embed-as-writer joins the cluster as a source:
writes pushed into a replication-source backlog, served to
subscribers (server replicas + embed read-replicas) over the same
wire protocol Phase 2 introduced.

- **new `kevy-scope` crate** — pure-data stone layer:
  `Scope` / `OwnershipTable` (longest-prefix routing + overlap
  linter + F4 fallback) / `MigrationTable`
  (start/commit/abort/lookup).
- **`kevy-config`** — `[cluster] scopes = "prefix=writer[|fallback],..."`
  flat-string parser (same shape rationale as v1.19's `peers`).
- **`kevy/src/scope_integration.rs`** — process-global ownership
  + peer-addr resolution + migration state + ingest guard +
  wire encoders.
- **`kevy/src/ops/scope_move.rs`** — `MOVE-SCOPE` +
  `MOVE-SCOPE-INGEST` cement (operator-issued; serialize prefix
  slice → ship via RESP2 → ingest with route bypass → commit/abort).
- **`kevy-cluster-rw::ReadWriteClient`** — follows `-MISDIRECTED`
  (per-key target cache, lazy conn cache) + retries on
  `-QUIESCED` (exponential backoff 5 ms → 80 ms, 7 attempts).
- **`kevy-embedded::replica_source`** — embed-as-writer TCP
  listener + accept loop + per-conn streaming threads. Reuses
  `kevy_replicate::source::ReplicationSource`.
- **`kevy-elect::ElectorSnapshot.down_peers`** — exposes per-peer
  liveness for F4 fallback decisions.

Wire shapes (Q3 = quiesce-window MOVE-SCOPE):
- `-MISDIRECTED writer is <host:port>` — final redirect
  post-migration commit.
- `-QUIESCED migrating to <host:port>` — transient during quiesce
  window; client backs off + retries against original primary;
  once committed, primary returns `-MISDIRECTED` and client
  follows.

Server-side bug fix: `dispatch.rs` GET/SET fast path was BELOW
the scope routing check; SET silently bypassed scope ownership.
Moved scope routing ABOVE the fast path (one Relaxed atomic load
per dispatch, below measurable noise per perfgate).

Anti-scope (locked): No Raft / gossip / online resharding /
MIGRATE-ASK. No write-shadowing during migration. No automatic
migration (operator-issued only). No cross-scope transactions.
Auto writer-reclaim deferred to v3.1 (v1.22 ships the manual
recovery procedure in `docs/cluster.md`).

Docs + example: `docs/cluster.md` "Scoped multi-writer" section;
`crates/kevy-embedded/examples/scoped_writer.rs` demonstrates the
embed-as-writer pattern.

---

### Phase 4 — `kevy-client-async`

Apps already on tokio / smol / async-std get a 1:1 async surface
with the blocking client plus pipeline-first batch sugar
(RFC Q4 part b) that collapses N sequential round-trips into one.
The blocking `kevy-client` stays the default and remains 0-dep;
async is opt-in.

- **new `kevy-client-async` crate** (v1.0.0, sole dep-rule
  exemption — RFC F5). 3 feature-gated transports:
  - `tokio` — `tokio::net::TcpStream`, default-features = false,
    minimum surface `["net", "rt", "io-util"]`.
  - `smol` — `smol::net::TcpStream`, default-features = false.
  - `async-std` — `async_std::net::TcpStream`. Each dep line
    carries an inline `# EXEMPTION — see
    feedback-pure-rust-no-c-principle.md` comment per the
    project's audit rule. T4.8 enforces exactly-one-runtime at
    compile time (`compile_error!` on zero or more than one).
    `default = ["tokio"]` as a dev convenience; lib consumers
    should set `default-features = false`.
- **Runtime-agnostic core.** Self-defined `AsyncRead` /
  `AsyncWrite` / `AsyncTransport` traits in the futures-io shape
  (`&mut [u8]`, `Poll<io::Result<usize>>`). Each runtime ships a
  thin per-type adapter that implements our traits on top of its
  `TcpStream`. No binding to `futures-io` / `tokio::io::AsyncRead`
  — that would bleed an ecosystem dep into the core.
- **`AsyncRespCodec<T>`** — async equivalent of
  `kevy_resp_client::RespClient`. Same state machine; reuses
  `kevy_resp::{encode_command, parse_reply}` so wire format has
  one implementation. `request` / `send` / `read_reply` /
  `pipeline` cover per-command and batched paths.
- **`AsyncConnection`** — TCP mirror of `kevy_client::Connection`.
  `open(url).await`, `from_transport(stream)`, plus 42 1:1 async
  methods across string / hash / list / set / sorted-set families.
- **`AsyncSubscriber`** — TCP mirror of
  `kevy_client::Subscriber`. connect / open / subscribe /
  psubscribe / unsubscribe / punsubscribe / recv / recv_message /
  hello3. `set_read_timeout` intentionally not mirrored — async
  timeouts live at the runtime layer.
- **`AsyncClusterClient`** — TCP mirror of
  `kevy_client::ClusterClient`. CLUSTER SLOTS topology discovery,
  one AsyncRespCodec per shard, CRC16 routing. 14 mirror methods.
- **Pipeline-first sugar.** `AsyncConnection::pipeline()` returns
  a typed-by-name builder (15 commands + `push_raw` escape).
  `run(&mut conn).await -> io::Result<Vec<Reply>>` — single TCP
  round-trip. Per-command errors surface as `Reply::Error(_)`
  inside the Vec. `into_cmds()` degrades cleanly onto a blocking
  client.
- **URL parser** — `kevy://` / `redis://` / `tcp://` schemes
  accepted. `mem://` / `file://` rejected with a pointer at the
  blocking client.
- **Examples** — `examples/tokio_hello.rs` +
  `examples/pipeline.rs`.
- **`docs/async.md`** — full guide. README gains an "As an
  async-runtime client" subsection.

---

### Tests + perfgate

- `cargo test --workspace -- --test-threads=4` → **1069 passed,
  0 failed** (was 996 at v1.20 baseline; +73 across P2 / P3 / P4).
- `cargo clippy --workspace --all-targets -- -D warnings` → clean.
  Per-runtime `--features {tokio,smol,async-std} --all-targets --
  -D warnings` clean under all three.
- New e2e: `server_replica_e2e` (P2, 3 tests), `embed_writer_e2e`
  + `scope_misdirected_e2e` + `scope_move_e2e` smoke (P3, 4
  tests), `tokio_basic` + `smol_basic` + `async_std_basic` (P4,
  5+4+4 tests).
- `bench_vs_blocking.rs` — 3 `#[ignore]` benches the operator
  runs against a live kevy server.
- lx64 perfgate PASS 6/6 on P3 commit `5649148` (scope routing
  added to dispatch hot path without measurable regression). P4
  perfgate by-construction (server / blocking client paths
  unchanged).

### Versions

- workspace `1.19.0` → `1.22.0`
- `kevy-embedded` `1.3.0` → `1.4.0` (P2 + P3 surface added)
- `kevy-client` `1.10.0` → `1.11.0` (P2:
  `Connection::Embedded(Box<Store>)` — pattern-matches need
  `Box`-aware adjustment; rebuild required)
- new crate `kevy-client-async` `1.0.0` (sole crates.io dep
  exemption — tokio / smol / async-std feature-gated)
- new crate `kevy-scope` `1.22.0`
- workspace `rust-version` pin removed — track the latest stable
  Rust toolchain (CI builds against current stable).

### Deferred to production-vet / v1.22.x

- T3.17 embed-writer-crash + fallback-takeover integration
  (F4 algorithm unit-tested in `kevy-scope`; multi-process elect
  integration left to actual deploys).
- Multi-shard replica upstream (currently 1 URL = 1 primary shard
  mirror).
- Replica snapshot ingest on offset-zero with rolled backlog.
- Auto writer-reclaim on F4 path (manual recovery shipped here).

## [v1.19.0] — 2026-06-19 (Phase 1.5 — automatic primary failover)

**v3-cluster Phase 1.5 — quorum-based automatic primary failover.**
Detection is by heartbeat every 200 ms; a peer is flagged DOWN after
5 s without a heartbeat; the alive replica with the highest
`repl_offset` (lowest `node_id` on tie) becomes a candidate and
broadcasts `OFFER`; on `N/2 + 1` `ACCEPT`s the candidate promotes
via the existing `REPLICAOF NO ONE` path and broadcasts `ANNOUNCE`.
Peers receiving `ANNOUNCE` retarget their `kevy-replicate` runner
at the new primary.

### Added

- **`kevy-elect` crate** — quorum failover layer on top of the v1.18
  manual `REPLICAOF` primitive. Pure-Rust 0-dep, RESP2 control plane
  over TCP (separate port per shard; election state is per-node).
  Public surface: `Transport::spawn(elector, hb_interval, listen,
  peers)`, `Transport::state_snapshot()`, `Transport::set_repl_offset()`,
  `Transport::shutdown()`.
- **Election state machine** (`Elector` struct): pure-logic core
  with `tick(now) → Vec<Outbound>` and `on_message(from, msg, now)`,
  exhaustively unit-tested against quorum / split-brain / dueling /
  rejoin / N=2 degenerate scenarios via an in-memory multi-elector
  simulator (`Sim`).
- **TCP transport**: one listener thread + one outbound thread per
  peer + one orchestrator thread, all interruptible via short
  read/accept timeouts (no Mutex on the hot path). Real-socket e2e
  test on loopback: 3-node primary kill → replica promotes in ~1 s.
- **`[cluster]` config extension**: `node_id`, `elect_port_base`,
  `peers = "id@host:port,..."` (flat-string shape, no parser
  extension needed). v1.18-era configs need no edit — kevy-elect is
  dormant unless both `node_id` and `peers` are set.
- **`ANNOUNCE` epoch handling**: a rejoining old primary sees a
  higher epoch on its first heartbeat to the new majority and
  demotes cleanly. No double-write — the partitioned minority never
  reached quorum so its writes had no durability guarantee.

### Anti-scope (locked)

Not Raft. No log replication consensus. No gossip discovery (peer
set is operator-declared). No cross-DC (RTT assumptions are LAN-
scale). No online membership change. No TLS / auth on the control
plane (consistent with v1.18 anti-scope).

### Recommendations

- **N ≥ 3** for any deployment that needs automatic failover. N=2 is
  intentionally locked when either node is down (config linter warns
  at startup).
- Tune `hb_interval_ms` × `down_after_ms` to your LAN's RTT; the
  defaults (200 ms / 5 s) assume sub-millisecond network.
- Use `READCONSISTENT` on the read side to avoid stale reads across
  a partition; the write side cannot retroactively repair minority
  writes.

### Documentation

- New "Automatic failover via kevy-elect" section in
  [`docs/replication.md`](docs/replication.md) — config, quorum
  table, split-brain protection, tunables.
- Full wire spec in
  [`crates/kevy-elect/docs/protocol.md`](crates/kevy-elect/docs/protocol.md).

### Tests

- 36 kevy-elect unit / sim tests (algorithm + 6 chaos drills via
  `Sim`).
- 1 real-TCP loopback e2e covering the 3-node primary-kill →
  promote path.

## [v1.18.0] — 2026-06-18

**v3-cluster Phase 1 — primary-replica replication + read/write split client.**
A kevy node can now run as a primary that streams every applied mutation to N
read replicas, or as a replica that connects to a primary and mirrors its
keyspace. Manual failover via `REPLICAOF` / `REPLICAOF NO ONE`. New companion
client `kevy-cluster-rw` splits writes to the primary and round-robins reads
across replicas.

### Added

- **Replication backlog + per-shard listener** (`[replication] role =
  "primary"`). Each applied mutation is encoded as a RESP envelope
  (`*2\r\n:<offset>\r\n<argv>`) and pushed into a per-shard bounded ring
  backlog; the reactor's pump streams frames out to connected replicas on
  each iteration. Per-shard listener binds at `listen_port_base + shard_id`
  (mirrors the cluster-listener pattern; per Issue Ledger I2). Tunable
  backlog size + reconnect-window slot retention.
- **Server-as-replica** (`[replication] role = "replica"` + `upstream =
  "host:port"`). At startup kevy spawns one runner thread per local shard,
  each holding a blocking `ReplicaClient` to the matching upstream shard
  port. Events flow to the shard's reactor over an MPSC channel and apply
  on the reactor thread under a `ReplicatedApplyGuard` (prevents chain-
  replication re-emit).
- **Snapshot ship** for fall-behind replicas. When a replica's `from_offset`
  is no longer in the primary's backlog (TooOld), the primary in-line-
  serializes the shard's keyspace via `kevy_persist::write_snapshot_to`,
  streams as `+SNAPSHOT` / `$<chunk>` / `+SNAPSHOT_END <ack_offset>`, and
  the replica loads via `kevy_persist::load_snapshot_from` then resumes on
  live frames with no gap.
- **`REPLICAOF host port`** / **`REPLICAOF NO ONE`** (alias `SLAVEOF`) — full
  dynamic retarget + demote. Stops in-flight runners (via `try_clone`'d
  socket + `Shutdown::Both` to break the blocking read), parses + resolves
  the new upstream, spawns fresh runner fleet. Effective role flips live;
  `ROLE` / `INFO replication` / `CLUSTER NODES` all report from live state,
  overriding static config.
- **`ROLE`** — Redis-shape reply. Master form: `["master", offset,
  [[ip, port, offset], ...]]` (per-replica array populated via the
  `getpeername(2)` capture added in this release). Slave form:
  `["slave", host, port, "connect", 0]`.
- **`INFO replication`** — full section with `role` / `connected_slaves` /
  `master_repl_offset` (master block) or `master_host` / `master_port` /
  `master_link_status` / `slave_read_only` / `slave_repl_offset` (slave
  block).
- **`kevy-cluster-rw::ReadWriteClient`** — companion client crate. Operator-
  supplied seed list (primary + replicas), one connection per node. Auto-
  routed `request` uses `is_write_verb` to dispatch; explicit `request_write`
  / `request_read(args, consistent: bool)` for tighter control. Replica
  fallback to primary when fleet empty or `consistent = true`.
- **Live-state plumbing**: process-global `replica_state` (senders + runners
  + upstream slot) so `REPLICAOF` can spawn/swap at runtime;
  `Commands::on_replication_view` hook publishes per-tick offset + connected
  count to the command layer.

### Anti-scope (locked, do not file issues for these in v1.18)

multi-master / cross-DC active-active / CRDTs / Raft / online resharding /
gossip discovery / AUTH / TLS / chain replication / non-RESP wire format for
replication. Automatic quorum failover (`kevy-elect`) is Phase 1.5 — **not**
in v1.18.

### Performance

Single-machine cluster perfgate on lx64 (Debian 13.1, 6.12 kernel, 16
hw threads) — all 6 baseline indicators PASS at the × 0.92 floor;
three of them exceed the recorded baseline outright. Replication
landing did NOT regress non-replication throughput on either reactor.
Reproduce with `bash bench/perfgate.sh <KEVY_BIN>`.

### v1.18 has no carved-out simplifications

Every follow-up the v3-cluster plan originally tracked as "lands in
v1.19+" was actually completed in v1.18: replica peer-addr capture
(T1.28.5), backlog watermark eviction (T1.22.5), background
snapshot serialization (T1.23.5), io_uring + replication (T1.12.5).

### Documentation

- New [`docs/replication.md`](docs/replication.md) — server + client
  recipes, REPLICAOF lifecycle, backlog tuning, simplifications + follow-ups.
- [`docs/cluster.md`](docs/cluster.md) extended with a read/write split
  section showing how cluster mode composes with replication.
- README v3-cluster section.

### Tests

937 workspace tests passing, 0 failures. Highlights:

- `crates/kevy/tests/replication.rs`: full handshake + streaming + snapshot-
  ship round trip + dynamic REPLICAOF lifecycle.
- `crates/kevy-cluster-rw/tests/rw_split.rs`: 1-primary + 2-replica
  ReadWriteClient matrix across every redis-type, READCONSISTENT, reconnect-
  within-backlog (no snapshot), reconnect-outside-backlog (snapshot).

## [kevy-client v1.9.0] — 2026-06-15

Independent `kevy-client` minor (workspace stays at 1.17.0): a **cluster-aware
client**, the ceiling fix for the multi-shard network tail latency a mailrs
dogfood run flagged.

### Added

- **`ClusterClient`** — discovers the topology via `CLUSTER SLOTS`, opens one
  connection per shard, and routes every key to its owner shard by CRC16 slot,
  so no command pays the server-side cross-shard forwarding hop. Requires the
  server in cluster mode (`--cluster`). Covers the standard surface: string
  (set/set_with_ttl/get/incr/incr_by/expire/persist/ttl_ms), hash/list/set/
  zset, multi-key del/exists (routed per key), keyspace-wide dbsize/flushall
  (the server fans these out internally), and ping/publish.

  Measured on a clean 16-core box (server cores 0-3, client cores 8-15):
  **conc64 533k ops/s @ p99 260µs**, vs a single shard's 333k @ 3858µs — 1.6×
  the throughput and a 15× tighter tail, by skipping the forwarding hop. The
  hop, not co-location or thread migration, was the dominant cost (each ruled
  out by measurement on the 4-vCPU dogfood box and the 16-core box).

## [v1.17.0] — 2026-06-14

Minor release: **network `INFO` observability** — the Memory, Keyspace, and
Stats sections now report the whole process rather than the single shard that
happened to answer, plus an API-naming footgun fix. Both from a mailrs dogfood
run of the kevy-server role. Workspace 1.16.0 → 1.17.0; kevy-embedded 1.1.20 →
1.2.0; kevy-client 1.7.16 → 1.8.0 (the `flush` → `flushall` rename below).

### Added

- **`INFO` cross-shard aggregation.** The server runs one independent store per
  shard and answers `INFO` on whichever shard the connection landed on, so the
  Memory / Keyspace / Stats numbers previously reflected ~1/Nth of the process
  (the same single-shard-view trap `DBSIZE` avoids by fanning out). A
  process-wide per-shard stats registry now lets `INFO` sum every shard's slot:
  - **`# Memory`** — `used_memory`, `used_memory_peak`, `evicted_keys` summed
    across shards (was a single shard's slice, often `0`).
  - **`# Keyspace`** — `db0:keys=N,expires=M,avg_ttl=0` (was empty).
  - **`# Stats`** — `total_commands_processed`, `total_connections_received`,
    `instantaneous_ops_per_sec` (Redis-style ring sampled over a ~1.6 s
    window), and `expired_keys` (all were stubbed `0`).
  Each shard publishes its gauges on the reactor tick and bumps command /
  connection counters in the hot path (one thread-local increment, atomics
  written only on the tick); the answering shard freshens its own slot from the
  live store first, so it is never stale.
- **`Store::expires` O(1) counter** — a live count of TTL-carrying keys backing
  `INFO keyspace`'s `expires=`, maintained at every TTL transition rather than
  an O(n) keyspace scan. A drift-guard test asserts it never diverges from the
  O(n) ground truth.

### Changed

- **`flush()` → `flushall()`** across `kevy_store::Store`,
  `kevy_embedded::Store`, and `kevy_client::Connection`. The old name read like
  `Write::flush` (sync-to-disk) but implemented Redis `FLUSHALL` (wipe every
  key + log it) — a data-loss footgun that cost a downstream debugging cycle.
  The new name matches the Redis command; a `#[deprecated]` `flush()` alias
  forwards for one release so callers migrate without a hard break.

## [v1.16.0] — 2026-06-12

Minor release: **COW persistence** — snapshot/rewrite serialization no
longer stalls a shard for the disk write (an O(n)-shallow view freeze,
~8 ns/entry, replaces it), plus an internal steel-dedup pass (one
crash-safe reshard engine shared by server and embedded), an embedded
durability fix, and real `INFO persistence` fields. Workspace 1.15.0 →
1.16.0; kevy-embedded 1.1.19 → 1.1.20; kevy-client 1.7.15 → 1.7.16 (dep
refs only). Perfgate PASS on every unit (6/6 angles, lx64; see
"Changed" for the gate-methodology update).

### Added

- **Background `BGSAVE` / `BGREWRITEAOF`**: the shard freezes a
  copy-on-write view of its keyspace (collection values are
  refcount-shared; mutations copy on write while a snapshot is in
  flight) and a per-shard background thread serializes it. `+OK`
  returns at the freeze; the snapshot/rewritten log swaps in within a
  tick (~100 ms) of the disk write finishing. One job in flight per
  shard (the Redis single-bgsave discipline). `SAVE` keeps its
  synchronous, blocking-durable contract — and is skipped with a log
  line if it races an in-flight background job.
- **`INFO persistence` real fields**: `aof_rewrite_in_progress` now
  reports the answering shard's actual state (it was a stubbed `0`),
  and the new `aof_rewrites_total` counts completed rewrites — the
  completion signal for the now-asynchronous BGREWRITEAOF. Refreshed
  per reactor tick.
- **`kevy_store::Store::collect_snapshot` / `SnapshotView`** (embedded /
  library users): an O(n)-shallow, `Send` point-in-time view —
  serialize on any thread while the store keeps mutating.
  `kevy_persist` serializers accept either a live store or a view
  (`SnapshotSource`).

### Changed

- **`BGSAVE` resets the AOF at the snapshot point** (replacing the old
  save-then-truncate): the new log carries exactly the post-snapshot
  writes, teed while the background save ran. Crash exposure is
  unchanged — the old log keeps receiving every write until the swap,
  and the snapshot-rename + log-swap commit happens in one adjacent
  critical section.
- **Embedded re-shard output is server-identical**: a shard-layout
  migration now writes per-shard `dump-{i}.rdb` snapshots + fresh AOFs
  (previously rewritten-in-place AOFs), and is crash-idempotent via the
  same `reshard.journal` roll-forward the server uses — a crash
  mid-migration previously lost the migrated state from disk. Backup
  rename failures now propagate instead of being silently ignored.
- **Perfgate methodology** (`bench/perfgate.sh`): each angle now
  measures 3 fresh server instances and gates on the median across
  instances (was 3 rounds against one instance). Instance-to-instance
  spread is the dominant noise axis (±5%); the baseline was re-recorded
  accordingly. Affects contributors only.

### Fixed

- **Embedded `Store::save_snapshot` no longer double-applies history on
  restart**: it never reset the AOF, so a restart with both files
  replayed the full log on top of the snapshot — duplicating
  non-idempotent commands (RPUSH'd elements doubled). It now performs
  the same tee'd log reset as `BGSAVE`; a save that races the
  background auto-rewrite waits it out (bounded) instead of writing a
  snapshot whose log would still double-apply.

### Internal

- One crash-safe reshard engine (`kevy_persist::reshard`) behind both
  the server and embedded migration paths; per-shard persistence file
  names have a single source of truth (`kevy_persist::layout`); the
  epoll/io_uring reactors share one cross-core drain
  (`drain_inbound_core`); the CLUSTER topology emitters share one
  derivation.

### Known limitations

- `BGSAVE` / `BGREWRITEAOF` completion is asynchronous: poll
  `INFO persistence` (`aof_rewrite_in_progress` / `aof_rewrites_total`)
  rather than expecting files to have swapped when `+OK` arrives.
- A collection first mutated while a snapshot is in flight is deep-
  copied at that moment (copy-on-write granularity is the whole
  collection) — a write touching a very large hash/zset during a
  background save pays that copy once.
- Tombstone-PEL, cross-shard XREADGROUP, and cross-slot multi-key
  items carried from v1.15.0 (below).

## [v1.15.0] — 2026-06-11

Minor release: **stream consumer-group / PEL persistence** (closing
v1.14.0's known limitation) plus a crash-safety batch from the v1.14
review. Workspace 1.14.0 → 1.15.0; kevy-embedded 1.1.18 → 1.1.19;
kevy-client 1.7.14 → 1.7.15 (dep refs only). Perfgate PASS on both
features (6/6 angles, lx64).

### Added

- **`XSETID key last-id [ENTRIESADDED n] [MAXDELETEDID id]`** (Redis 7
  shape): overwrite a stream's scalar state. Write-classified
  (AOF-propagated) and keyspace-notifying (class `t`); errors mirror
  upstream ("requires the key to exist", "smaller than the target stream
  top item").
- **Snapshot format v4**: each `OP_STREAM` payload now carries the
  stream's consumer groups — group `last_delivered_id`, consumers with
  `last_seen_ms`, and the full PEL (owner, `delivery_time_ms`,
  `delivery_count`), including tombstone rows for XDEL'd-while-pending
  entries. v2/v3 snapshots still load.

### Fixed

- **Consumer groups / PELs now survive every persistence path** (was the
  v1.14.0 known limitation): snapshots (v4 group section), AOF rewrites
  (`XGROUP CREATE`/`CREATECONSUMER` + one `XCLAIM … TIME t RETRYCOUNT n
  FORCE JUSTID` per live PEL row — full delivery fidelity, upstream's own
  rewrite technique), and reshards (the redistribution path carries
  groups). Previously SAVE-only persistence, BGREWRITEAOF, and layout
  re-shards all dropped group state.
- **AOF rewrite scalar drift**: a stream whose tail (or entirety) had
  been XDEL'd replayed with a rolled-back ID clock — and an empty stream
  (deleted-out or groups-only) vanished from the rewrite entirely. The
  rewrite now re-creates empty streams (`XADD … MAXLEN 0` + the new
  `XSETID`) and restores `last_id` / `entries_added` /
  `max_deleted_entry_id` exactly.
- **Server reshard is crash-idempotent**: new snapshots are written under
  temp names and a durable `reshard.journal` marks the commit point
  before any source file is touched; an interrupted migration is rolled
  forward on the next start. Previously a crash inside the migration
  window left the data dir empty (recovery only by hand from
  `.premigration` backups).
- **io_uring dead-conn block waiters**: EOF / write-error / protocol-
  error now cancels a conn's BLPOP/XREAD waiters immediately instead of
  on the 1/16-throttled reap — a parked waiter on a dead conn could
  consume a pushed element meant for a live client for up to 16
  iterations.
- **Embedded / server data-dir interop**: a meta-less multi-shard dir
  opened by the embedded store at `shards = 1` silently loaded shard 0
  only; the shard count is now inferred and the dir migrated whole.
  Default-named single-shard embedded dirs also record `shards.meta`
  (custom `with_aof_filename` / `with_snapshot_filename` names are a
  documented interop opt-out).

### Known limitations

- AOF **rewrites** drop tombstone PEL rows (pending entries whose stream
  entry was XDEL'd) — they can't be re-created by command replay, and
  kevy's XCLAIM/XAUTOCLAIM treat them as reapable. Snapshots (v4)
  preserve them fully; only XPENDING visibility across a
  rewrite-then-restart is affected.
- Multi-stream `XREADGROUP` across shards executes per shard: if one
  shard errors (e.g. NOGROUP) after another delivered, the deliveries
  stand (visible in XPENDING, reclaimable via XAUTOCLAIM) while the
  client sees the error. Upstream pre-validates; documented trade-off.
- Cross-slot multi-key commands execute (single-machine superset) instead
  of returning `-CROSSSLOT`; keyspace-wide views stay whole-keyspace on
  every port (carried from v1.14.0).

## [v1.14.0] — 2026-06-10

Major release: **single-node CLUSTER mode** (key-aware routing — the last
lever of the perf-ceiling campaign), the full hot-path perf campaign (①
allocator/parse/dispatch, ② reactor notification), cross-shard XREADGROUP,
and a TTL-reaper fix. 8-shard headline moves from ~8.7 M to **30.8 M GET /
22.3 M SET ops/s** (pinned-hashtag angle, lx64). Workspace 1.13.0 → 1.14.0;
kevy-embedded 1.1.17 → 1.1.18; kevy-client 1.7.13 → 1.7.14.

### Added

- **Single-node cluster mode** (`--cluster` / `KEVY_CLUSTER=1` /
  `[cluster] enabled`): keys route by Redis-cluster slot (CRC16 `{hashtag}`
  & 16383, one contiguous range per shard); every shard `i` binds a second
  deterministic listener at `port_base + i` (default `port+1+i`) answering
  wrong-shard keys with `-MOVED`. Stock cluster-aware clients
  (`redis-cli -c`, `redis-benchmark --cluster`, client libraries) discover
  the topology and talk straight to the owning shard — no cross-shard
  forwarding tax. The main SO_REUSEPORT port keeps full proxy-style
  behaviour. `CLUSTER SLOTS / SHARDS / NODES / INFO / MYID / KEYSLOT /
  COUNTKEYSINSLOT` answer with the real topology; `KEYSLOT` matches upstream
  (`foo` → 12182), and a packet capture across a full benchmark run shows
  zero spurious MOVEDs.
- **`shards.meta` v2 + automatic re-shard**: the data dir now records
  (shard count, routing scheme); a mismatch at bring-up re-homes every key
  once, with `.premigration.<ts>` backups. Fixes the server silently
  stranding keys on a `--threads` change (it never wrote a meta), and an
  embedded shrink-to-one bug that could truncate a live AOF.
- **`kevy_hash::crc16` / `key_hash_slot`**: XMODEM CRC16 (compile-time
  tables, slice-by-4) + Redis-cluster hashtag slot mapping.
- **Cross-shard non-blocking multi-stream `XREADGROUP`**: previously only
  the first STREAMS key's shard was read, silently dropping streams owned
  elsewhere; now fans out per stream with group context, PEL updates and
  AOF logging on each owning shard (logged as the single-stream rewrite, so
  per-shard replay is correct).
- Fuzz targets for `shards.meta` parsing (round-trip fixpoint) and
  `key_hash_slot` (slot range + hashtag metamorphic property).

### Changed

- **Hot-path perf campaign** (carried since v1.13.0): ArgvPool zero-malloc
  cross-shard forwarding, SmallReply stack-inline replies, borrowed
  single-pass multibulk parse, tier-1 GET/SET dispatch fast path,
  DispatchMeta resolve-once, single conns-probe pre-dispatch, io_uring
  spin→nap→park idle ladder (idle CPU 6.5 % → 0.7 %), batched
  uring_arm_conns, IORING_OP_TIMEOUT bounded park.
- **SLOWLOG defaults to OFF** (`slowlog-log-slower-than = -1`): the 10 ms
  Redis default cost every command an `Instant::now()` pair (~13-19 % at
  multi-M ops/s). Re-enable with `CONFIG SET slowlog-log-slower-than 10000`.
- **TTL reaper bounds its bucket walk** (`samples × 8` visits per round):
  a TTL-free keyspace previously paid a full-table walk every 100 ms tick
  (measured 6 % of server CPU); sparse-TTL coverage leans on the rotating
  random start + lazy expiry.
- `CONFIG GET` now exposes `save` (empty = no save points), so standard
  tooling (e.g. redis-benchmark's per-node config fetch) stops warning.

### Fixed

- A bare 1-element `XREADGROUP` could panic the receiving shard
  (out-of-bounds argv index); now a clean arity error.
- Cluster port ranges that would overflow u16 are rejected at startup
  (loudly) instead of wrapping onto low ports while CLUSTER SLOTS
  advertises 65536+.
- XREADGROUP-gather write housekeeping derived the stream key by scanning
  for the literal "STREAMS", mis-targeting WATCH/notify when a group or
  consumer is named "streams"; now derived from the fixed rewrite shape.
- Cluster mode with AOF off and an empty dir now still records the layout,
  so a later SAVE + non-cluster restart can't silently strand keys.

### Known limitations

- Stream **consumer groups / PELs are not encoded** into snapshots or AOF
  rewrites (pre-existing): they recover only via original-AOF command
  replay, so SAVE-only persistence, BGREWRITEAOF, and layout re-shards drop
  group state (originals remain in `.premigration` backups). Tracked for an
  upcoming release.
- Cross-slot multi-key commands execute (single-machine superset) instead
  of returning `-CROSSSLOT`; keyspace-wide views stay whole-keyspace on
  every port.

## [v1.13.0] — 2026-06-09

Minor release: **cross-shard keyspace scan** for embedded sharding. Workspace
1.12.0 → 1.13.0; kevy-embedded 1.1.16 → 1.1.17; kevy-client 1.7.12 → 1.7.13.
Reported by mailrs (shard-scan gap blocking `with_shards` adoption).

### Added

- **`Store::collect_keys(pattern, limit)`** — `KEYS`/`SCAN`-glob across **every
  shard**. With `with_shards(n > 1)`, the `with(|s| s.collect_keys(..))` escape
  hatch only saw shard 0, so a glob scan (key bust, metrics gauges) silently
  missed `(n-1)/n` of the keyspace. `collect_keys` is the cross-shard,
  read-locked replacement; identical to the old `with(...)` call when
  `shard_count() == 1`. `limit` bounds the total across shards.
- **`Store::for_each_shard(f)`** — run `f` against each shard's underlying
  `kevy_store::Store` (the cross-shard escape hatch for ops not yet wrapped),
  and **`Store::shard_count()`**. Single-key work still uses `with_key`; globs
  use `collect_keys`.

## [v1.12.0] — 2026-06-09

Minor release: **shared-nothing keyspace sharding for embedded mode** — the
embedded store now scales reads across cores. Workspace 1.11.0 → 1.12.0;
kevy-embedded 1.1.15 → 1.1.16; kevy-client 1.7.11 → 1.7.12.

### Added

- **`Config::with_shards(n)`** — partition the embedded keyspace into `n`
  shared-nothing shards (`hash(key) % n`, the same router the network server
  uses), each an independent lock + keyspace + AOF. Concurrent operations on
  different shards never contend, so a multi-threaded embed consumer scales
  across cores. Measured on a 16-core box (in-memory GET, 10 threads):
  **5.3M ops/s (single mutex, v1.10.0) → 12.5M (RwLock, v1.11.0) → 66.3M
  (16 shards) — 12.5× over the campaign, and positive scaling (21M @1 thread
  → 66M @10) where the unsharded store regressed with thread count.**
  - **Default `n = 1`** — the original single-lock / single-`aof-0.aof` layout,
    zero behavior change, zero migration. Sharding is strictly opt-in.
  - With `n > 1`, persistence uses per-shard `aof-{i}.aof` + a `shards.meta`
    file. The first open at `n > 1` re-shards a legacy single AOF into per-shard
    files (the old file is backed up to `aof-0.aof.premigration.<ns>`); changing
    the shard count re-shards via a temp keyspace. Pub/sub is process-wide
    (handled on shard 0), not sharded.
  - `Store::with_key(key, f)` — the `with` escape hatch routed to a key's shard
    (plain `with` targets shard 0).

## [v1.11.0] — 2026-06-09

Minor release: embedded read-path performance — GET throughput and multi-core
read scaling. Workspace 1.10.0 → 1.11.0; kevy-embedded 1.1.14 → 1.1.15;
kevy-client 1.7.10 → 1.7.11. All measured on a 16-core Linux box.

### Changed

- **GET no longer reads the clock for keys without a TTL.** The per-access read
  path called `is_expired_at(Instant::now())`, evaluating the monotonic clock
  on every access even when the key had no deadline. It now reads the clock
  only in the has-deadline branch. **No-TTL GET ~+51%** (embedded in-memory,
  single thread: 19.1M → 28.9M ops/s).
- **TTL'd-key GET uses a coarse cached clock** (Redis `mstime` model): a clock
  refreshed once per reactor batch (server) / reaper tick (embedded background)
  instead of an `Instant::now()` per access. Writes still stamp deadlines from
  a fresh clock, so deadlines stay exact (a key expires at most one
  refresh-interval late, never early). Opt-in per store — only the server
  reactor and the embedded *background* reaper, which refresh the cache, trust
  it; manual-reaper / bare-`Store` reads a fresh clock so lazy expiry still
  works without an explicit tick. **TTL'd GET ~+62%** (17.7M → 28.7M ops/s),
  now on par with no-TTL GET.
- **Embedded `Store` uses a `RwLock`; GET takes a shared read lock.** A
  multi-threaded embed consumer previously serialized every access on one
  exclusive mutex — throughput *regressed* with thread count (16-core: GET
  20.0M @1 thread → 5.3M @10). GET now takes a read lock + a non-mutating
  lookup (when `maxmemory == 0`), so concurrent readers run in parallel:
  **10-thread GET 5.3M → 12.5M ops/s (+136%)**. Expired keys are reclaimed by
  the active reaper / next write rather than lazily on read (read returns
  `None` either way); with eviction on, GET keeps the exclusive + LRU-stamping
  path.

### Added

- `cargo run -p kevy-embedded --example bench_embed[_mt]` — single- and
  multi-threaded in-process throughput harnesses.

## [v1.10.0] — 2026-06-09

Minor release: the embedded auto-AOF-rewrite is now **non-blocking**, plus a
push-style metric callback — closing the two gaps left from the mailrs feedback
(`kevy-product-feedback-2026-06-09`). Workspace 1.9.0 → 1.10.0; kevy-embedded
1.1.13 → 1.1.14; kevy-client 1.7.9 → 1.7.10.

### Changed

- **Embedded background auto-AOF-rewrite no longer blocks application writes.**
  v1.9.0 ran the auto-rewrite inline under the store lock (blocking writers for
  the full serialize + disk write + fsync). It now runs in three phases: (1)
  serialize the keyspace to memory under the lock and start teeing live appends
  into a diff buffer, (2) **release the lock** and spill the snapshot image to
  disk + fsync — the expensive part, off the hot path, (3) re-take the lock
  briefly to append the tee'd diff and atomically swap the file in. Writes
  during the disk spill are captured by the tee, so nothing is lost; crash
  safety is unchanged (atomic `rename`). The manual `Store::rewrite_aof()` stays
  synchronous (the explicit "rewrite now" path); a manual call is a no-op while
  a background rewrite is in flight.

### Added

- **`Config::with_metric_sink(callback)`** — a push-style metric callback that
  fires `KevyMetric::Replay { commands, bytes, elapsed_ms }` after startup AOF
  replay and `KevyMetric::Rewrite { keys, before_bytes, after_bytes,
  elapsed_ms }` after each AOF rewrite. For continuous monitoring without
  polling `info()`. `KevyMetric` is `#[non_exhaustive]`.

## [v1.9.0] — 2026-06-09

Minor release: AOF maintenance + observability for embedded mode, from the
mailrs production feedback (`kevy-product-feedback-2026-06-09`). Workspace
1.8.1 → 1.9.0; kevy-embedded 1.1.12 → 1.1.13; kevy-client 1.7.8 → 1.7.9.

### Added

- **Automatic AOF rewrite in embedded mode.**
  `Config::with_auto_aof_rewrite(pct, min_size)` triggers a `BGREWRITEAOF`-style
  compaction when the live AOF has grown `pct` percent past its size at the
  previous rewrite and is at least `min_size` bytes — defaults **100 % /
  64 MiB**, matching Redis and the network server. The check rides the
  background reaper tick (or `Store::tick` in manual reaper mode). The manual
  `Store::rewrite_aof()` already existed and is unchanged.
- **Embedded introspection API.**
  `Store::info() -> KevyInfo` (keys, used_memory, aof_bytes, expire_pending,
  evictions, expired_keys), `Store::expire_pending_count()` (live keys carrying
  a TTL — the expire-set size), and `Store::ttl(key) -> Option<Duration>` (an
  ergonomic wrapper over the raw `ttl_ms` PTTL sentinels). Backed by a new
  `kevy_store::Store::ttl_pending_count()`.
- **`docs/persistence.md`** — AOF / snapshot / fsync policy / TTL semantics /
  rewrite & compaction / crash recovery / file-naming / embedded introspection,
  in one place. Linked from the README.

### Changed

- **AOF replay now logs its wall-clock time**: `… replayed N commands from M
  bytes in T ms (clean)`. Replay time scales with the AOF, so surfacing it
  gives operators a baseline to watch.

## [v1.8.1] — 2026-06-09

Patch release: **TTL deadlines now survive a restart.** Workspace 1.8.0 →
1.8.1; kevy-embedded 1.1.11 → 1.1.12; kevy-client 1.7.7 → 1.7.8. Reported by
the mailrs production deployment (INC-2026-06-09).

### Fixed

- **A key's TTL was reset to a fresh full duration on every restart.** TTL was
  persisted as a *relative* `PEXPIRE <ms>` in the AOF (and as remaining-ms in
  the binary snapshot), so AOF replay / snapshot load re-anchored the deadline
  to load-time. A key set with a 300 s TTL, after a restart hours later, came
  back with a fresh 300 s instead of expiring at its original instant — so a
  cache entry could outlive its intended lifetime indefinitely across frequent
  restarts (it never expired from the reader's point of view). In-memory TTL
  (within a single process lifetime) was always correct; only persistence was
  affected.
  - **All persistence paths now record an absolute Unix-ms deadline.** The
    embedded `set_with_ttl`/`expire` log `PEXPIREAT`; the server's AOF append
    follows a relative TTL write (`EXPIRE`/`PEXPIRE`/`SETEX`/`PSETEX`/
    `SET … EX|PX`) with an absolute `PEXPIREAT` correction; `BGREWRITEAOF`
    emits `PEXPIREAT`; the binary snapshot stores the absolute deadline
    (format v3). Load/replay subtracts elapsed wall-clock and drops keys whose
    deadline already passed.
  - Backward-compatible: a v2 snapshot (relative TTL) and old relative
    `PEXPIRE` AOF entries still load (treated as relative-from-load, the prior
    behaviour) — no migration needed; new writes are absolute.

### Added

- **`EXPIREAT` / `PEXPIREAT` commands** (absolute Unix-time expiry, matching
  Redis). Single-key routed; replicated to the AOF. These are also the wire
  form the persistence layer now uses internally.

## [v1.8.0] — 2026-06-07

Minor release: io_uring is now the default reactor on Linux, with an
automatic epoll fallback. Workspace 1.7.0 → 1.8.0; kevy-embedded 1.1.10 →
1.1.11; kevy-client 1.7.6 → 1.7.7.

### Changed

- **The Linux reactor now auto-selects io_uring at startup, falling back
  to epoll when the host can't build a ring.** Previously io_uring was
  opt-in via `KEVY_IO_URING=1`; epoll was the default. Now an unset
  `KEVY_IO_URING` probes io_uring (creates + drops a real ring with the
  production parameters, including the buffer-ring registration) and uses
  it when available — otherwise it logs the reason and uses epoll.
  **Startup never fails over reactor choice.** This catches a
  seccomp-blocked `io_uring_setup` (Docker's default profile) and
  pre-5.19 kernels before any shard loads data.
  - Override still honoured: `KEVY_IO_URING=0/off/no/false` forces epoll;
    any other value forces io_uring with no fallback (a setup failure then
    surfaces loudly — for benchmarks / tests).
  - The startup line reports the choice: `kevy: reactor = io_uring
    (io_uring available)` or `... = epoll (io_uring unavailable …)`.

### Fixed

- **io_uring disconnect leaked block waiters and pub/sub registrations.**
  The io_uring reactor's connection reaper hand-rolled its teardown
  (removed the conn + unsubscribed channels only), skipping the shared
  `close_conn` path the epoll reactor uses. So disconnecting a connection
  that was parked on a cross-shard `BLPOP`/`XREAD` left its arbiter waiter
  and `psub` registrations behind — a later `RPUSH`/`XADD` could wake the
  gone waiter and consume an element meant for a live client. The reaper
  now routes through `close_conn` (which runs `drop_for_conn`,
  `cancel_xshard_on_close`, channel + pattern unsubscribe). Only reachable
  under io_uring; epoll was always correct.

## [v1.7.0] — 2026-06-07

Minor release: cross-shard multi-stream `XREAD`. Workspace 1.6.1 → 1.7.0;
kevy-embedded 1.1.9 → 1.1.10; kevy-client 1.7.5 → 1.7.6.

### Fixed

- **Non-blocking `XREAD … STREAMS s1 s2 …` over streams on different shards
  returned partial data.** It routed to the first STREAMS key's shard only,
  so streams owned by other shards were silently dropped (no error). It now
  fans each stream out to its owning shard and merges the replies in request
  order — empty streams skipped, `*-1` when all empty, `COUNT` applied per
  stream, `$` resolved on each stream's owning shard. Single-stream `XREAD`
  keeps the fast single-shard path; blocking `XREAD` already parks on the
  origin shard via the cross-shard BLOCK arbiter (v1.5.0).
  - `XREADGROUP` multi-stream cross-shard remains a follow-up (its `>`
    consume semantics need separate handling).
  - Additive internal API only (a new `Route::XReadGather` variant); no
    public breakage.

## [v1.6.1] — 2026-06-07

Patch release: faster snapshots. Workspace 1.6.0 → 1.6.1; kevy-embedded
1.1.8 → 1.1.9; kevy-client 1.7.4 → 1.7.5. No public API change.

### Changed

- **Snapshot / BGREWRITEAOF bulk writes use a 1 MiB BufWriter** (was the
  8 KiB default). `SAVE` was measured at only ~12 % of disk sequential
  bandwidth (758 MB/s vs a 6.1 GB/s NVMe ceiling on an M4 Pro) — the small
  buffer turned a multi-hundred-MB snapshot into tens of thousands of small
  `write(2)`s. The larger buffer lifts SAVE to **~1.73 GB/s (+128 %)**.
  Content is byte-identical; only the flush granularity changes.

## [v1.6.0] — 2026-06-07

Minor release: AOF `appendfsync always` group commit. Workspace 1.5.1 →
1.6.0; kevy-embedded 1.1.7 → 1.1.8; kevy-client 1.7.3 → 1.7.4.

### Added / Changed

- **AOF group commit for `appendfsync always`.** Previously every write
  fsynced individually (`flush()+sync_data()` per command). Now a pipelined
  batch's writes are buffered and fsynced once at the batch boundary — still
  before that batch's replies leave the shard, so the "durable before reply"
  contract is unchanged. Measured **+46 %** (0.89M → 1.30M SET/s, `-c50
  -P16`, 10 shards, lx64 NVMe); the per-write-durable vs 1-second-window
  gap shrank from −39 % to −8 %. Applies to all always-write paths on both
  reactors (epoll + io_uring local reads, and the cross-shard request
  batch). `everysec` / `no` / cache-only paths are unchanged.
  - New public API on `kevy_persist::Aof`: `begin_group()` / `end_group()`
    (additive; existing embedders recompile unchanged).

### Verified

- New `kevy-persist` test `aof_group_commit_defers_then_flushes` (the batch
  is not on disk until `end_group`, then fully durable). Full workspace
  tests + clippy green; compat3 differential 135/135 vs valkey 9.1 + redis
  7.4. Regression A/B (lx64): no GET/SET hot-path change; 3-way still leads
  (kevy io_uring ~2.2× valkey / ~1.7× redis). See `bench/REPORT.md`.

## [v1.5.1] — 2026-06-07

Patch release: three valkey-parity / correctness fixes surfaced by
extending the cross-engine differential harness (`bench/compat3.sh`) to
Streams / Geo / blocking / RENAME — now 135/135 vs valkey 9.1 + redis 7.4,
and gated in CI. All three are pre-existing (not v1.5.0 regressions); no
public API change. Workspace 1.5.0 → 1.5.1; kevy-embedded 1.1.6 → 1.1.7;
kevy-client 1.7.2 → 1.7.3.

### Fixed

- **Cross-shard `RENAMENX` could lose the source key.** When source and
  destination hashed to different shards and the destination already
  existed, step 1 took the source off its shard but the NX-refused step-2
  put was never rolled back — the reply `:0` was correct but the source
  key was gone. The refused put now hands the value back and the
  orchestrator restores it on the source's shard before replying (a new
  `RenameStep::Restore`), so a no-op `RENAMENX` no longer loses data.
- **`XGROUP` / `XINFO` were unusable on a multi-shard server.** Their
  stream key is at `args[2]` (after the subcommand) but they routed by
  `args[1]` (`CREATE`/`STREAM`), landing on the wrong shard — `XGROUP
  CREATE` failed with "key doesn't exist" and `XREADGROUP`/`XACK`
  cascaded. Now routed by the real key (keyless `HELP` forms stay local).
- **`GEOHASH` / `GEOPOS` diverged from valkey in the last digit(s).**
  The 11th `GEOHASH` char spilled the low score bits instead of
  zero-padding like Redis; `GEOPOS` decoded the cell centre with a
  float-op order that rounded differently than Redis's `(min+max)/2`.
  Both now reproduce valkey byte-for-byte. Adds kevy-geo unit tests (the
  existing ones only checked the first 10 geohash chars).

## [v1.5.0] — 2026-06-07

Minor release: cross-shard blocking pops. A `BLPOP` / `BRPOP` / `XREAD
BLOCK` whose key lived on a shard other than the connection's used to hang
the client forever; multi-key `BLPOP` was rejected outright. Both are now
fixed via a cross-shard BLOCK arbiter (`kevy_rt::block_xshard`). New
`Commands` hooks are additive with default bodies, so embedders recompile
unchanged. Workspace bump 1.4.2 → 1.5.0; kevy-embedded 1.1.5 → 1.1.6;
kevy-client 1.7.1 → 1.7.2 (both inherited the workspace bump, no API
change).

### Added

- **Cross-shard blocking pops (v2-7e).** `BLPOP` / `BRPOP` / `XREAD BLOCK`
  / `XREADGROUP BLOCK` now work when watched keys live on shards other
  than the connection's, and multi-key `BLPOP k1 k2 …` is supported
  (previously rejected). The connection parks on its origin shard and
  watch registrations fan out to each key's owning shard; the origin is
  the sole arbiter, so no target shard ever pops speculatively (which
  would lose data when two keys go ready at once). See
  `kevy_rt::block_xshard`. New additive `Commands` hooks
  (`block_serve_argv`, `block_ready`, `wake_idx`) default to no-op, so
  embedders recompile unchanged.

### Fixed

- A single-key `BLPOP` / `BRPOP` / `XREAD BLOCK` whose key hashed to a
  shard other than the connection's **hung the client forever** — the
  command was forwarded to the key's shard as a plain dispatch, which on
  an empty list returned a 0-byte reply and never parked, woke, or timed
  out. Now it parks correctly via the cross-shard arbiter. Regression
  test `blocking_cross_shard::blpop_remote_key_times_out_not_hang`
  (nshards = 8).

### Known gaps

- Non-blocking multi-stream `XREAD` across shards still reads only the
  first STREAMS key's shard (a missing-feature, not a hang) — a separate
  cross-shard gather, tracked for a follow-up.

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
