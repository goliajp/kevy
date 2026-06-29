# Finding — v1.33.0 replication chaos test fails to fire on Linux (lx64)

Date: 2026-06-30 (v1.33 post-ship investigation)
Anchor: [`crates/kevy/tests/crash_replication_followed.rs`](../crates/kevy/tests/crash_replication_followed.rs)

## Observation

Running `cargo test -p kevy --test crash_replication_followed --release -- --ignored --nocapture` on:

- **Mac aarch64**: 410 k primary-ACKs in 3 s, replica catches ~12 % (88 % observational lag), strict NO CORRUPTION assertion passes.
- **lx64 (Linux x86_64)**: **0 primary-ACKs in 3 s**, test fails the vacuous-test assert. Primary's `kevy.stderr.log` is CLEAN (no error, only startup line + AOF replay). Replica's log is also clean — NO replication-handshake attempts logged (no "connect to ... failed", which on Mac fires many times before catching up).

## Interpretation

The chaos test framework (Harness, WriterPool, pipelined verify) works correctly on Linux — the `crash_always`, `crash_everysec`, and `crash_during_rewrite` tests all pass on lx64 with high throughput. The Linux-specific failure is **isolated to the `[replication]` setup path** in the chaos harness.

Two leading hypotheses:

1. **Primary's accept queue not draining for WriterPool conns**: The harness's `wait_ready` PING succeeds (proving primary accepted ONE conn). But the subsequent 4 WriterPool threads cannot connect — all 4 `TcpStream::connect` calls fail silently and the writer threads `return` without doing work. Possibly:
   - The replication's listener-side bind interacts with the main socket accept in some Linux-specific way under io_uring.
   - SO_REUSEPORT routing differs on Linux (default `--accept-shards=None` should mean all shards accept; with `--threads 1` there's only 1 shard, so this should be fine).
2. **Silent TOML rejection**: lx64's kevy might be silently rejecting the `[replication]` section despite no error log. The replica's lack of "connect to" attempts suggests its `[replication]` was ignored entirely.

Neither hypothesis is confirmed without source-instrumentation of kevy's replication startup path on Linux.

## What this is NOT

- NOT a corruption finding. kevy never returned wrong data on Linux; the test framework's strict NO CORRUPTION assert never had a chance to fire because 0 ACKs accumulated.
- NOT a v1.33.0 ship-blocker. v1.33.0 ships with the test passing on Mac. The Linux-specific issue is deferred to v1.33.x investigation.
- NOT necessarily a kevy bug. May be a test config issue (wrong port numbering convention, missing replica handshake delay, etc.).

## Mac aarch64 finding (still valid)

From v1.33.0 ship-time:
- Primary 410,948 ACKs in 3 s.
- Post-SIGKILL + 2 s drain: replica 49,691 present / 361,257 lost / 0 corrupted.
- 88 % observational replication-lag at sustained ~137 k SET/s.
- NO CORRUPTION strict assert passes.

This is a real production-relevant finding — kevy replication falls behind under sustained high-write load. Investigation roadmap (per v1.33.0 CHANGELOG entry):
- Audit kevy-replicate streaming + replica drain rate.
- Check primary's replication backlog size (default 256 MiB).
- Cross-platform comparison (this finding doc).

## Next steps

- v1.33.x: investigate the Linux-specific test failure (debug print primary's TOML reading, add INFO replication query, manually drive a replication handshake from the test).
- v1.34: pivot to sustained-load soak (1 h+ stability) — different chaos category, less dependent on replication path.
- v1.35+: kevy-elect chaos coverage (promote-replica-on-primary-death).

The chaos test framework continues to be the right substrate — it surfaced both the Mac 88 % lag finding AND the Linux test-config gap. Both are valuable production-relevant data.
