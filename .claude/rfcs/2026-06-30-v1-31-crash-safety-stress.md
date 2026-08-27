# RFC v1.31 — crash-safety stress test scaffolding (industrial-grade testing, step 1)

**Status**: planning lock 2026-06-30, implementation starts immediately
**Theme**: v2 = kevy 工业级。v1.x 是过程。**NOT 补特性**, 主要把测试标准提起来。v1.31 = step 1 of 5 (crash safety; subsequent v1.3x cycles cover 并发 / 锁 / 竞争 / 多写)。
**Author**: GOLIA K.K.
**Date**: 2026-06-30

## Why crash-safety first

Of the 5 industrial-grade testing categories (并发 / 锁 / 竞争 / 多写 / 断电), 断电 (crash safety) is:
1. **Highest blast radius for users** — silent data loss after a confirmed write is the worst-possible KV failure mode.
2. **Most concrete contract**: "every ACK'd write survives restart" (or document the precise lost-window).
3. **Existing AOF + snapshot infrastructure already shaped** ([`crates/kevy-persist`](../../crates/kevy-persist)): there's a tail-truncation tolerance test, a corrupt-snapshot tolerance test. Missing: **abrupt SIGKILL during concurrent writes**.
4. **Detectable**: the test outcome is binary (present / absent), not statistical.

After v1.31, v1.32+ tackles concurrency stress (sustained load + sanity invariants).

## L1 lock — version line

- v1.31.0 = crash-safety stress test scaffolding + first concrete test.
- Pure testing infra. **No new server features**. No API breaks.
- Per user direction (round 33): "v2 要 kevy 完全工业级的水平，v1.x 都是过程，但不要轻易补特性，主要是各种测试标准要提起来，并发，锁，竞争，多写，断电等等"

## Scope

### In-scope (v1.31.0)

- **New crate `kevy-chaos`** (or `tests/chaos/` integration tests, TBD by simplicity) hosting the chaos test harness.
- **Test 1 — concurrent-writers + abrupt-SIGKILL → restart → verify ACK'd writes present**:
  - Start kevy with AOF `appendfsync = always` (the strictest durability contract)
  - Spawn N writer threads, each doing M `SET key_i value_i` sequentially, capturing each `+OK` ACK
  - At time T (mid-flight), abrupt SIGKILL on the kevy process
  - Restart kevy on the same data dir
  - For each captured ACK, `GET` the key and verify the value matches
  - **Pass criterion**: every captured ACK reads back the expected value. ZERO data loss tolerated for `always` fsync.
- **Test 2 — concurrent-writers + abrupt-SIGKILL + AOF `everysec` → bounded lost-window**:
  - Same as above but `appendfsync = everysec`
  - **Pass criterion**: at most a 1-second lost window of ACK'd writes (the documented `everysec` contract). Strict ACK'd-but-lost beyond 1s is a bug.
- **Run config**: `cargo test --workspace --release -- --ignored` opts in (these tests take seconds to minutes). Default `cargo test` skips them.
- **Doc**: `docs/chaos-tests.md` — how to run, what each test asserts, how to reproduce a failure.

### Out-of-scope (v1.31.0 — deferred to v1.32+ or later)

- io_uring SQ ring abandonment race (in-flight SQEs at SIGKILL — kernel handles correctly per io_uring spec, but a test would assert no ACK is sent for those).
- Replication failover under crash (primary dies, replica takes over).
- Multi-writer kevy-scope race tests.
- Loom enumeration coverage for inbox / replication state machines (kevy-rt's existing `loom.rs` is the seed; expand later).
- Sustained-load soak (1 hour stability run; v1.32).
- TSan / ASan integration.
- Property-based testing / fuzz coverage.

## Implementation order (Cn commits)

C1 — `crates/kevy-chaos/` skeleton + Cargo.toml (path-dep only, 0 crates.io as always). Lib re-export of `ChaosRunner` API.
C2 — `kevy-chaos::Harness` — spawn kevy child process, redis-cli/TCP write loop, capture ACKs, SIGKILL, restart, verify.
C3 — Test 1 (always fsync zero-loss) `tests/crash_always.rs`.
C4 — Test 2 (everysec bounded-window) `tests/crash_everysec.rs`.
C5 — `docs/chaos-tests.md` — invocation + assertion table.
C6 — workspace 1.30.0 → 1.31.0 + CHANGELOG + tag + ship.

Each commit: `cargo test --workspace --lib` green; chaos tests gated `--ignored` so they don't slow default CI.

## Risks

- **R1 — Test flakiness on slow CI runners**: SIGKILL timing is delicate. Mitigation: assert minimum N successful writes BEFORE SIGKILL (else the test is meaningless), then SIGKILL only after that count is reached.
- **R2 — io_uring restart contention**: kevy AOF + io_uring SQ ring. If the process is SIGKILL'd mid-flight, kernel-side io_uring SQE accounting is bookkept by kernel; restart should see consistent on-disk state per fsync. If not, that's a real bug — the test surfaces it.
- **R3 — Test runtime**: even ~10s per test × 2 tests × N runs in CI is bounded. Acceptable for `--ignored` opt-in. CI gating in v1.32+.

## Naming / convention

- Crate: `crates/kevy-chaos/` (lib only, no bin). Per project convention `kevy-*` prefix.
- 0-dep: standard library + `kevy-resp-client` (path-dep) for the TCP client + `std::process::Command` for the child kevy + std signal sending via `libc::kill` if needed via `kevy-sys`. No new crates.io deps.
- LOC: keep harness ≤ 500 LOC per project rule; per-test files ≤ 300 LOC.

## Out-of-scope clarification

This is **NOT** v1.31 = "add a new command" / "add a new wire protocol feature" / "refactor X". It's **purely testing scaffolding**. The kevy server code may need 0 changes for v1.31; if a test surfaces a real bug, that bug-fix is a separate sub-task (v1.31.x) outside the RFC's main scope.

Per user round-33 direction: "**不要轻易补特性**, 主要是各种测试标准要提起来"。This RFC is precisely that — testing standard raise, not feature.

## Decision summary

v1.31.0 = `crates/kevy-chaos` crate + 2 chaos tests (always-fsync zero-loss / everysec bounded-window) + doc + ship. ~300-400 LOC harness + ~150 LOC × 2 tests + 1 doc. No server code changes (unless a test surfaces a bug; treat that as v1.31.x patch). Step 1 of 5 toward v2 industrial-grade testing standard.
