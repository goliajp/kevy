# RESOLVED — the "primary never came up" phantom was an availgate script bug

**Status: closed.** Root-caused on lx64 (x86 Linux, kernel 6.12,
io_uring). It was **not** io_uring, not the reactor, not replication,
not a kevy hang at all. availgate reported a healthy server as dead
because the gate ran its PING through a binary that does not exist on a
release-only build.

## The bug

`708c0f8a` ("bound every CLI call") wrapped the CLI var *before* the
executable check:

```sh
CLI=target/release/kevy-cli
command -v timeout && CLI="timeout 15 $CLI"   # CLI is now a 3-word string
[ -x "$CLI" ] || CLI=target/debug/kevy-cli    # tests the whole string as a
                                              # path → fails → falls back
```

`[ -x "timeout 15 target/release/kevy-cli" ]` tests that entire string
as a single path, which never exists, so it fell back to
`target/debug/kevy-cli`. CI (and any release-only build) has no debug
binary, so **every PING ran a missing command and failed**, and the
gate declared "primary never came up" while the primary was serving
fine.

Fix: resolve the binary first, then wrap.

```sh
CLI=target/release/kevy-cli
[ -x "$CLI" ] || CLI=target/debug/kevy-cli
command -v timeout && CLI="timeout 15 $CLI"
```

## Why it looked Linux-only (and fooled every earlier hypothesis)

macOS has no coreutils `timeout`, so `command -v timeout` failed, the
CLI was never wrapped, `[ -x target/release/kevy-cli ]` passed, and the
real release CLI ran — green. Only Linux (CI + lx64) has `timeout`, so
only Linux wrapped, fell back, and "hung". Every earlier read — io_uring
gap, reactor-independent hang, primary+replication race, the ss/wchan
"evidence" — was the healthy server's normal park (`io_cqring_wait` /
`ep_poll` = waiting for a connection that the broken CLI never made).
The lesson: when a symptom is "server X unreachable", verify the CLIENT
before autopsying the server.

## Verification

lx64, release build, fixed availgate: all 16 clamps green (READONLY,
lag convergence, slave0 truth, link down/up, min-replicas, crash
failover, WAIT, read-your-writes, bounded staleness, quorum lease).
Before the fix: 8/8 "primary never came up" on the same box. Folded
availgate + repligate back into the hard `contract gates` job.
