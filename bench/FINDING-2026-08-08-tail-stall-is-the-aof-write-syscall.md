# Both tail-stall classes are the same syscall: write(2) to the AOF, on the reactor thread

V3 Phase A. The tailgate baseline showed two RED cells (mixed:
reactor gap up to 6.07s; firehose: 1.3–2.3s). One six-cell
discriminator matrix — 60s per cell, one knob per cell, the
in-process prober reporting client p99.9 and the reactor's own
`reactor_tick_gap_max_us` side by side — collapses both to a single
mechanism.

## The matrix (lx64, io_uring reactor, 4 shards)

| cell | client p99.9 | client max | reactor gap max |
|---|---:|---:|---:|
| mixed baseline | 6.9ms | 486ms | **3.06s** |
| mixed, auto-rewrite off | 0.5ms | 583ms | **2.62s** |
| mixed, --no-aof | 0.3ms | 13ms | 310ms |
| firehose baseline (everysec) | 363ms | 1.44s | **1.52s** |
| firehose, appendfsync=no | 39ms | 3.12s | **3.97s** |
| firehose, --no-aof | 4.3ms | 5.4ms | 49ms |

## What the columns rule out

* **Not the rewrite.** Turning the auto-rewrite off leaves the mixed
  stall intact (2.62s vs 3.06s). The 4GiB-single-frame crash the
  first tailgate run found in the rewrite was real — and fixed — but
  it was never the stall.
* **Not the giant-list realloc, mostly.** --no-aof still runs the
  same single-key LPUSH storm to a multi-GiB list; the reactor gap
  drops to 310ms. (That residual is the secondary target — still over
  the 100ms bar, plausibly the list's doubling memcpy.)
* **Not fsync policy — worse, it's not even fsync.** appendfsync=no
  removes every fsync from the append path and the reactor stall got
  BIGGER (3.97s), while the client p99.9 improved 10× because the
  stalls got rarer. That trade is the kernel's signature: with no
  fsync back-pressure, dirty pages accumulate until writeback
  throttling blocks the NEXT write(2) for seconds — rarer, bigger.

## The mechanism

The reactor thread appends the AOF inline with the dispatch batch
(`aof_begin_group` / `aof_end_group` around `dispatch_batch`). At
600MB–1GB/s sustained ingest the buffered `write(2)` itself becomes
the blocking point: the kernel's dirty-page ratelimiter parks the
writing thread — the reactor — until writeback drains. Every fsync
cadence merely reshapes when that parking happens.

Redis has the same anatomy (main-thread write, bio-thread fsync) and
the same documented failure under sustained ingest; "move the fsync
off-thread" is necessary but NOT sufficient, because the stall is in
write(2), not fsync(2).

## The fix's shape (RFC, not this finding)

The append must leave the reactor thread entirely. kevy already owns
an io_uring reactor: AOF appends as chained SQEs (or a dedicated
writer thread with a bounded queue) keeps the reactor non-blocking
while preserving group-commit semantics for `always` (reply gated on
the append's CQE, not on a synchronous write). See RFC
2026-08-08-v5-v3-aof-offload.

## Instrument notes

* The gauge and the prober agree everywhere (gap ≥ client max in
  every cell) — the reactor self-observation is trustworthy.
* Client p99.9 alone would have MISRANKED fsync=no as a fix (39ms!)
  while the reactor gauge shows it makes the underlying stall worse.
  Two views, one story: keep both in tailgate permanently.
