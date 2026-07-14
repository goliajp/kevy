# primary+replication server bring-up hangs on the GitHub x86 runners

**Status:** open, tracked. availgate + repligate live in a
`continue-on-error` CI job (`replication-gates`) so the hang is visible
without masking CI or blocking the branch (user decision 2026-07-14).
Not a shipping-path blocker.

**NOT io_uring** (corrected): the first cut blamed io_uring and forced
epoll (`KEVY_IO_URING=0`), but the epoll run hung identically — `wchan`
showed the shards in `ep_poll`, not `io_cqring_wait`. It is
reactor-independent. It is **primary+replication specific**: a
standalone server is green in the docker smoke; the two gates that hang
(availgate, repligate) each start a real primary AND a real replica.

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
(`runtime_run.rs`). The `listener` exists and is registered.

**ss evidence (the decisive one).** The socket-layer autopsy on a
failing epoll run:

```
State  Recv-Q Send-Q Local Address:Port  users
LISTEN 0      1024   127.0.0.1:7381      kevy pid=2769 fd=20
LISTEN 0      1024   127.0.0.1:7381      kevy pid=2769 fd=17
LISTEN 0      1024   127.0.0.1:7381      kevy pid=2769 fd=14
LISTEN 0      1024   127.0.0.1:7381      kevy pid=2769 fd=11
LISTEN 0      1024   127.0.0.1:17381     kevy pid=2769 fd=12
```

All four client listeners (SO_REUSEPORT on 7381) are LISTENing, plus
the replication listener on 17381 — the server is fully up. **Recv-Q =
0**: nothing is stuck in the accept queue. So the client PING is either
never reaching the listener, or is accepted and then the connection
stalls after accept (no read). Next witness needed: an ESTABLISHED-conn
`ss -tnp` (does the PING conn exist? is its Recv-Q filling — data
arrived but not read?), and whether `--threads 1` (no SO_REUSEPORT, no
cross-shard) changes it.

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
