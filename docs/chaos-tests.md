# Chaos tests — kevy industrial-grade testing standard, step 1

> **v1.31 ships this**: the `kevy-chaos` test harness + two concrete
> tests that surface crash-safety contracts. **No new server features**
> are added. v1.x is process toward v2 = full industrial-grade.

## What's here

Two `--ignored`-gated integration tests in `crates/kevy/tests/`:

| Test file | Server config | Strict assert | Observational |
|-----------|---------------|---------------|---------------|
| `crash_always.rs` | `appendfsync = always` | every ACK'd write reads back the ACK'd value after restart (ZERO loss) | — |
| `crash_everysec.rs` | `appendfsync = everysec` | NO CORRUPTION (every present read matches its ACK'd value) | lost-fraction reported (not strict-failure) |

## Running

Build kevy release first (chaos tests need the actual binary, not
`cargo run`):

```sh
cargo build --release -p kevy
cargo test -p kevy --test crash_always --release -- --ignored --nocapture
cargo test -p kevy --test crash_everysec --release -- --ignored --nocapture
```

Or set `KEVY_BIN=/path/to/kevy` if the binary lives elsewhere.

These tests take seconds to minutes per run (the harness spawns kevy,
runs writers for 2–5 s, kills + restarts, then GETs every captured
ACK). They're gated `--ignored` so default `cargo test` skips them
(unsuitable for fast inner-loop runs).

## What the tests check

### `crash_always` — zero-loss contract
- Spawn kevy with `--threads 2 --appendfsync always`
- 4 writer threads do `SET wN_kS wN_vS` for 2 s, capturing each
  `+OK` ACK
- Abrupt `SIGKILL` (via `Child::kill`)
- Restart kevy on the same data dir
- GET every captured ACK, assert value matches

**Strict assert**: ZERO loss. If a single ACK'd write is missing or
returns a wrong value after restart, the test fails. This is the
canonical durability contract for `always` fsync; if it breaks, it's
a real bug.

### `crash_everysec` — no-corruption + observational lost-fraction
- Same shape as `crash_always` but `appendfsync = everysec` and 5 s
  pre-kill duration (gives ≥ 4 fsync windows)
- After restart, count `present` / `lost` / `corrupted` of the
  captured ACKs

**Strict assert**: `corrupted == 0`. Every key whose value is
non-nil after restart must equal the ACK'd value. kevy must NEVER
return a wrong value, regardless of fsync policy.

**Observational metric**: lost-fraction is logged but not
failure-bound. Empirically the lost-fraction at high write rate
(~117 k SET/s) is higher than the naive "≤ 1 s window" expectation;
see [`bench/PERF-FINDING-2026-06-30-everysec-loss-fraction.md`](../bench/PERF-FINDING-2026-06-30-everysec-loss-fraction.md)
for the finding + hypotheses pending v1.31.x investigation.

## What the chaos crate provides

`crates/kevy-chaos` exports:

- `Harness::spawn(HarnessConfig)` — spawn kevy as a child process via
  `--config <toml>`, wait for `PING` ack (or 10 s timeout).
- `Harness::kill(KillSignal)` — `Sigkill` for chaos / `Sigterm` for
  graceful-shutdown comparison.
- `Harness::restart()` — kill + respawn on the same data dir.
- `WriterPool::spawn(port, n_writers, stop)` — N TCP writer threads
  doing `SET key value` in a loop; each captures `(key, value, seq)`
  for every `+OK` reply into a shared `Arc<Mutex<Vec<AckEntry>>>`.
- `verify_all_present(port, &acks)` — GETs every captured ACK and
  returns Err on first mismatch.
- `pick_free_port()` — ephemeral port helper for parallel-test
  isolation.

Test-only crate; 0 crates.io deps (path-dep on `kevy-resp-client`
only).

## What's NOT here (deferred to v1.32+)

Per the v1.31 RFC, this is **step 1 of 5** toward v2 industrial-grade
testing. Future cycles add:

- **v1.32** — Sustained-load soak: 1 h+ stability run, watch for
  memory growth / latency drift / lost wakeups.
- **v1.33** — Cross-shard race coverage: expand the existing
  `kevy-rt/tests/loom.rs` model to inbox + replication state machines.
- **v1.34** — Multi-writer chaos: `kevy-scope` per-prefix writer
  contention, lost-write detection.
- **v1.35** — Replication failover under crash: primary dies mid-
  replication, replica takes over, verify slot-by-slot consistency.

These are the user-stated 5 categories (并发 / 锁 / 竞争 / 多写 / 断电).
v1.31 covers 断电 (crash safety) at step 1.

## Bug reporting

If a chaos test surfaces a bug:
1. Capture the test output (`--nocapture`).
2. Note kevy version, kernel + OS, and the random seed (`AckEntry.seq`
   is deterministic per writer).
3. File against the v1.31.x patch line; chaos-test-surfaced bugs are
   v1.x's reason to exist.
