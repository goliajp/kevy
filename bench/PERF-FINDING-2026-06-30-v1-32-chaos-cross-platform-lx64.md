# Finding — v1.32 cross-platform chaos validation: kevy on Linux x86_64 (lx64) vs Mac aarch64

Date: 2026-06-30 (v1.32 chaos test cross-platform validation)
Anchor: chaos tests (`crates/kevy/tests/crash_always.rs`, `crash_everysec.rs`) run on lx64.

## Setup

Same chaos test harness shipped in v1.31.2. Mac aarch64 vs Linux x86_64 (lx64) comparison.

- Mac: macOS aarch64 (M-series), kqueue reactor (epoll fallback in `Runtime`), kevy `--threads 2`.
- Linux: lx64 (x86_64, 16-core baremetal), io_uring reactor, kevy `--threads 2`.

Both: kevy v1.31.2 release binary, 4 concurrent writer threads, `appendfsync = always` or `everysec`, abrupt `SIGKILL`, restart, pipelined-verify every captured ACK.

## Results — `crash_always` (zero-loss strict contract)

| Target | ACKs / wall-clock | SET/s sustained | Strict assert | Wall-clock |
|--------|-------------------|----------------:|---------------|-----------:|
| Mac aarch64 | 530 / 2 s | ~265 | 0 lost / 0 corrupted ✓ | 2.22 s |
| **lx64 x86_64** | **373,317 / 2 s** | **~187,000** | **0 lost / 0 corrupted ✓** | **2.31 s** |

**Linux is ~700× higher SET/s for `appendfsync = always` than Mac.** Root cause: Mac's `fsync` performs `F_FULLFSYNC`-class behavior (APFS guarantees flush to non-volatile storage on every call, expensive); Linux `fdatasync` only forces the file's data + metadata to disk (cheaper). kevy's policy semantics are identical on both — the platform-level fsync cost diverges, not the kevy code path.

## Results — `crash_everysec` (no-corruption strict + observational lost-fraction)

| Target | ACKs | SET/s sustained | Present / Lost / Corrupted | Loss fraction | Wall-clock |
|--------|-----:|----------------:|----------------------------|--------------:|-----------:|
| Mac aarch64 | 622,382 | ~125,000 | 622,040 / 342 / **0** | **0.055 %** | 5.28 s |
| **lx64 x86_64** | **1,611,660** | **~322,000** | **1,611,452 / 208 / 0** | **0.013 %** | **6.08 s** |

**Linux is 2.6× faster AND has tighter loss-fraction than Mac on `everysec`.** Both far below the naive "1 s window ≈ 20 % lost" expectation. The lost-fraction is bounded by the userspace BufWriter capacity at SIGKILL time, not by the everysec timer cadence.

Per-shard replay summary (lx64 example):
```
kevy: AOF /tmp/kevy-chaos-everysec-37773/aof-1.aof replayed 805,653 commands
  from 37,020,288 bytes in 168 ms; trailing 16 bytes were a partial frame
  (crash mid-append, recoverable)
```

The "trailing N bytes were a partial frame (crash mid-append, recoverable)" recovery is the same code path as the existing `aof_truncated_tail_is_tolerated_on_restart` unit test — empirically validated under chaos conditions.

## What this validates

- **kevy is consistently industrial-grade across both platforms.** The crash-safety contracts hold (zero loss for `always`, near-zero for `everysec`).
- **kevy is much faster on its production target (Linux + io_uring) than on the dev target (Mac + kqueue).** The platform delta is in the kernel I/O stack, not in kevy.
- **The chaos test framework works cross-platform** — same harness, same assertions, runs on both Mac aarch64 and Linux x86_64 without modification.
- **No platform-specific bugs surfaced.** Loss-fraction is dominated by the BufWriter capacity at SIGKILL time (constant ~8 KB per shard); Linux's higher throughput moves more data through the pipeline in the same wall-clock, so the constant BufWriter loss is a SMALLER fraction of total writes.

## Headline correction

Combining v1.31.2 + v1.32 findings:

> **kevy's `appendfsync = everysec` on Linux (production target) loses ~0.013 % of ACK'd writes on abrupt SIGKILL+restart at sustained 322 k SET/s.**
>
> This is the actual industrial-grade durability number, replacing the v1.31.0/v1.31.1 ship-time-write-up "86 %" hypothesis (test bug) AND the v1.31.2 Mac-only 0.05 % number (correct but slower platform).

## Next step (v1.33 / v1.32.1)

- **AOF rewrite race chaos test** — BGREWRITEAOF + concurrent writes + SIGKILL mid-rewrite → verify both the pre-rewrite + post-rewrite writes survive. This catches a known Redis-historical-bug class.
- **Replication failover under crash** — primary dies mid-replication, replica takeover, verify all confirmed-on-primary writes made it to replica before kill.
- **Sustained-load soak** (1 h+ stability) — leak / drift / lost-wakeup detection over long horizon.
