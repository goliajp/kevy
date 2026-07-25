# Deep pipeline of big-arg values wedged the connection — FIXED

**Status: RESOLVED.** Root-caused by instrumentation, fixed in four
commits, and covered by a new `uringgate` case that fails deterministically
on the pre-fix binary. Pre-existing (the v1.29 big-arg path), orthogonal to
the capacity arc — found while running the arc's B6 capacity phase.

## Symptom

A 512-deep pipeline of `SET` with values at or above the 4 KiB big-arg
promote threshold intermittently wedged the connection: the client blocked
on a reply forever, every reactor thread sat in `io_cqring_wait` at 0 % CPU,
and a fresh connection to the same server answered `PING` normally (so: a
per-connection wedge, not a stuck reactor). Roughly **one wedge per two
runs of 120 k keys**. It reproduces on a plain non-tiered server, and
demotion never engages before it hits — nothing to do with tiering.

## Root cause (found by instrumenting, not by reasoning)

Three rounds of source-level reasoning each found and fixed a *real* race
in the big-arg cancel cycle, and each pushed the wedge later (1 k → 4.6 k →
11 k → 111 k keys) without closing it. Extending the stall dump to name the
big-arg sub-state settled it in one run:

```
big_arg=Frame(3232/4132) recv_armed=false arm_queued=false
in_arm_pending=false cancel_pending=false read_pending=false rearm_recv=false
```

A **`Frame`** stitch — not the BareSet cancel/read cycle everything had been
aimed at — waiting for 900 more bytes with no armed recv and nothing queued
to re-arm one.

Only the BareSet cancel/read cycle *owns* recv mode: it cancels the
multishot and reads the body itself. The `Frame` variant (cross-shard
bare-`SET`, `SETEX`/`APPEND`/`MSET` — i.e. the common path on a multi-shard
instance, since most keys hash to another shard) stitches its bytes from the
**ordinary multishot**. But the arm pass gated the re-arm on
`pending_big_arg.is_none()`, so a conn in `Frame` could never re-arm. When
the multishot ended on its own — buffer-ring `ENOBUFS`, routine under a deep
pipeline — the frame never completed, and with no pending SQE and no output
the conn dropped out of the arm queue permanently.

`uring_on_recv`'s `suppress_rearm` already drew the line at exactly the two
BareSet states. **The two sites were inconsistent**; that inconsistency was
the bug.

## The fixes

| commit | what |
|---|---|
| `5f9f57fe` | cancel ack completes the cycle when the multishot already vanished (`-ENOENT` path) |
| `c9aa93c8` | a big-arg SQE the ring refused keeps the conn in the arm queue (same trap `recv_arm_deferred` already covered) |
| `af8fe07d` | a non-`ECANCELED` multishot terminal (`ENOBUFS`/EOF) also ends the cancel cycle's target side |
| `7ad17124` | **the wedge**: only the BareSet states gate the recv re-arm; `Frame` keeps its multishot |

The first three are genuine races on their own paths and stay; the fourth is
the one the symptom was about.

## Verification

- **20 runs × 120 k keys = 2.4 M pipelined 4 KiB SETs: zero wedges** (before:
  ~1 wedge per 2 runs).
- `kevy-rt` + `kevy` suites green on Linux.
- New `uringgate` case `big-arg-pipeline` (512-deep SETs at 4 KiB and 8 KiB
  per round, every reply under the gate deadline, conn must still serve):
  - fixed binary → **PASS**, 25 rounds in 5.3 s
  - pre-fix binary → **FAIL at round 8**, `bap: reply 39/512 (4096B values)`,
    triage `SHARD ALIVE, per-conn wedge`

## Notes for next time

- **Instrument after two rounds that don't close.** Three source-reasoned
  fixes moved the symptom without solving it; one instrumented run named it.
  The stall dump printing only `big_arg=true` could not tell a legitimately
  in-flight read from a wedge — that missing detail cost the three rounds.
- **A macOS build does not compile the uring code** (`cfg(target_os = "linux")`
  throughout). Local `cargo build` passing means nothing for this subsystem;
  lx64 or CI is the gate. One commit here was pushed with a broken call site
  that only the Linux build caught.
- The capacity envelope's B6 load stays byte-bounded (~128 KB in flight).
  That is realistic bulk-load client behaviour and keeps the capacity phase
  measuring capacity; the deep-pipeline contract now has its own gate case.
