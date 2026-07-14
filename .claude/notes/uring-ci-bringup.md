# io_uring server bring-up hangs on the GitHub x86 runners

**Status:** open, worked around (contract gates forced to epoll,
`KEVY_IO_URING=0`, same as covgate). Not a shipping-path blocker — the
default reactor auto-selects and this is a CI-runner-environment gap.

## Symptom

On `ubuntu-latest` (x86_64) GitHub runners, a kevy server started under
the io_uring reactor prints its two startup lines and then never
accepts a client connection — `availgate` reported `primary never came
up` and the whole contract-gates job failed. First surfaced only after
the SPOP fix let the contract job run to completion instead of being
cancelled by concurrency, so it had been masked, not introduced.

## Evidence

The thread-state autopsy added to `bench/availgate.sh` (walks
`/proc/<pid>/task/*/wchan` on failure) caught the primary alive but
mute:

```
tid 2555 wchan=futex_do_wait      stat=S   # main thread
tid 2558 wchan=hrtimer_nanosleep  stat=S
tid 2560 wchan=futex_do_wait      stat=S
tid 2561 wchan=io_cqring_wait     stat=S   # shard 0
tid 2562 wchan=io_cqring_wait     stat=S   # shard 1
tid 2563 wchan=io_cqring_wait     stat=S   # shard 2
tid 2564 wchan=io_cqring_wait     stat=S   # shard 3
```

All four shard reactor threads are parked in `io_cqring_wait` — they
submitted their SQEs (multishot accept among them) and are blocked
waiting for a completion that never arrives. The client's SYN is not
producing an accept CQE.

`arms_accept` is not the cause: availgate passes no `--accept-shards`,
so `accept_shards` is `None` and every shard has `arms_accept = true`
(`runtime_run.rs`). The `listener` exists, the multishot accept SQE is
prepped at the top of `run_uring`'s loop.

## What it is not

- **Not `5d6133b0` (inbox wake) or `e5106a1d` (prep split).** Container
  A/B on arm64: HEAD and both reverts each pass bring-up 25/25. The
  primary in availgate has no replica inbox, so those changes are
  no-ops on it.
- **Not reproducible off the x86 runner.** arm64 Linux container: 25/25
  clean. macOS (kqueue): every clamp green. `docker --platform
  linux/amd64` on an arm64 host runs x86 *user* space on the host's
  arm64 kernel, so it does not exercise x86 io_uring at all. lx64 (the
  self-hosted x86 box) is currently down; x86 Linux is cross-compile
  only, which cannot run.

## Prior related finding (context, in `uring_reactor.rs`)

`run_uring` already carries a fix for a *different* primary-under-
io_uring hang: a blocking replication listener whose first `accept()`
stalled the shard (`rl.set_nonblocking()`). That one is fixed; this is
not it (the shards are parked in the ring, not blocked in a syscall).

## Next steps when a healthy x86 Linux host exists

1. Reproduce with `KEVY_IO_URING=1` and confirm `ss -tlnp` shows the
   listener's accept queue (`Recv-Q`) filling — i.e. SYNs land but no
   accept CQE fires. (A hook for this dump belongs in the autopsy.)
2. Bisect the accept path: does a **single-shot** accept (re-armed per
   CQE) work where multishot hangs? That would isolate a multishot
   accept / kernel-version interaction.
3. Check the runner kernel's io_uring feature set vs what
   `build_uring` assumes (multishot accept is 5.19+, provided-buffer
   ring likewise). A partially-supported feature that neither errors at
   setup nor delivers CQEs would fit this exactly.
