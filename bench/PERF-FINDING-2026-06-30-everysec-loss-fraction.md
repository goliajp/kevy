# ⚠ WITHDRAWN — finding was a TEST BUG, not a kevy bug

> **Status**: WITHDRAWN as of v1.31.2 (2026-06-30). The "86 % lost-
> fraction" reported below was due to ephemeral-port exhaustion in the
> chaos test's per-GET TCP connect loop, NOT a kevy fsync issue.
>
> After fixing the chaos verify to use a single pipelined TCP
> connection, the real loss-fraction is **0.05 % (342 of 622 k ACKs
> lost)** — vastly better than the naive "1 s window ≈ 20 %"
> expectation. kevy's `appendfsync = everysec` implementation is
> excellent. See the v1.31.2 CHANGELOG entry for the corrected
> empirical conclusion.
>
> The text below is kept verbatim for the historical trail
> (the v1.31.0 / v1.31.1 ship-time write-up).

---

# Finding — `appendfsync = everysec` lost-fraction far above 1-second window expectation

Date: 2026-06-30 (v1.31 chaos test scaffolding round 34)
Anchor: [`crates/kevy/tests/crash_everysec.rs`](../crates/kevy/tests/crash_everysec.rs)

## Empirical observation

Running the v1.31 chaos test scaffolding (`cargo test -p kevy --test crash_everysec --release -- --ignored`) on local Mac aarch64:

- kevy `--threads 2`, `appendfsync = "everysec"`, no auto_aof_rewrite override
- 4 concurrent SET writer threads
- 5 s pre-kill duration → 588 828 ACK'd SETs (~ 117 k SET/s sustained)
- abrupt `SIGKILL`, then restart on same data dir
- Verification: each ACK'd write read back via `GET`

Result:
- 81 472 keys present (13.8 %)
- **507 356 keys lost (86.2 %)**
- **0 keys corrupted** ← strict invariant holds
- Test passes (corruption is the strict assert; lost-rate is observational)

## Why this is unexpected

The naive everysec contract: at most 1 s of writes lost. At 117 k SET/s steady state, 1 s = 117 k writes ≈ 20 % of total in a 5 s run.

Empirical 86.2 % lost ≈ **4.3 s of writes lost out of 5 s** — 4 × the naive bound.

## Hypotheses (pending v1.31.x investigation)

1. **everysec fsync deferral under sustained write load** — the kevy bio thread + AOF sync handler may not fire the periodic fsync on a fixed 1-second cadence under heavy enqueue pressure. The `EverySec` enum branch in `kevy-persist` is called from the shard tick path; if shards are 100 % busy on writes, the tick interval drifts.
2. **auto_aof_rewrite race** — kevy's auto_aof_rewrite_pct triggers when AOF size exceeds threshold. At 117 k SET/s × ~30 bytes per RESP frame = ~3.5 MB/s, the default 64 MiB threshold takes ~18 s to hit; not relevant at 5 s. **Eliminated** as a candidate for the 5 s test.
3. **AOF write buffering** — kevy may accumulate writes in a userspace buffer that only flushes on the fsync timer. SIGKILL discards the buffer; if the buffer is large at kill time, many writes are lost despite being ACK'd to the client.
4. **ACK-before-write-to-AOF race** — if kevy ACKs the SET reply BEFORE the AOF write hits the kernel page cache (vs. only after fsync), the ACK becomes a "soft promise" with semantics weaker than the everysec contract advertises.

## What to investigate (v1.31.x)

- Audit `crates/kevy-persist` for the everysec timer cadence and the
  write→ACK→AOF-flush ordering.
- Check whether the AOF write is queued to bio thread BEFORE the
  reply is sent (the conservative valkey/redis-compat ordering) or AFTER
  (a perf optimization that weakens durability).
- If the bio thread is the bottleneck, measure its actual queue depth
  at peak load and the lag between SET completion and on-disk arrival.
- Reproduce on Linux lx64 (the production-like target). Mac aarch64
  fsync semantics may differ but the in-process behavior should match.

## What is OK at v1.31.0

- **No corruption** (verified strict): kevy never returns a wrong value, even after abrupt kill mid-AOF-write.
- **Restart succeeds** consistently.
- The `appendfsync = always` contract is empirically zero-loss
  (separate test `crash_always.rs`, 100 % present-ratio after kill).
- The chaos test scaffolding (`kevy-chaos` crate) is working as intended
  — it surfaces the kind of question v1.31 is meant to surface.

The v1.31.0 ship is the test infrastructure + a passing strict-everysec-no-corruption test + this finding doc. **The lost-rate question is a real product-relevant question that needs investigation, not a v1.31.0 ship-blocker.**
