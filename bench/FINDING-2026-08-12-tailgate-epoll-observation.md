# FINDING 2026-08-12 — tailgate on the epoll reactor: an observation round

The v5.1 closeout asked for the numbers S3 left unmeasured: the tail
bars have only ever been run on the io_uring default, while the
epoll/kqueue path is the old-kernel fallback, the macOS main path, and
the whole PR test matrix. **No bar is set on this cell** — the point is
to know the shape, not to gate on it.

Box (lx64), 16 cores, NVMe via `TMPDIR=$HOME/captmp`, 4 threads,
default config (AOF on, everysec, offload on), median of 3 runs per
cell, both reactors measured the same day off the same build
(`bugfix/availgate-promotion-window-repro`, which no gate here
touches).

| cell | metric | uring (default) | epoll (`KEVY_IO_URING=0`) |
|---|---|---:|---:|
| mixed | PING p99.9 | 8.15 ms | **4.68 ms** |
| mixed | reactor tick gap | 44.96 ms | **440.29 ms** |
| firehose | PING p99.9 | 12.58 ms | **116.95 ms** |
| firehose | reactor tick gap | 55.91 ms | **774.49 ms** |

uring PASSES both cells against the 100 ms bars. epoll is over on three
of the four numbers.

## What the numbers say, and what they do not

**The firehose is a genuine client-visible gap on epoll**: 117 ms at
p99.9 against 12.6 ms on the ring, ~9× worse, on the cell built to be
the hardest AOF shape. S3 moved the AOF writes and fsyncs off the epoll
reactor, which is why this is 117 ms rather than the seconds the
pre-S1 shape produced — but client I/O itself is still one
`epoll_wait` + `read`/`write` per event on the reactor thread, and the
ring path issues those as queued operations.

**The mixed cell is a divergence worth naming rather than
explaining.** epoll's client latency is BETTER than the ring's
(4.68 ms vs 8.15 ms) while its tick gap is ~10× worse (440 ms vs
45 ms). Those two facts cannot both be "epoll is slower". The tick gap
measures the interval between the reactor's housekeeping branches, and
a saturated poll loop can serve clients promptly while starving
housekeeping — plausible, and unverified. The honest statement today
is: **on the everyday shape, epoll serves clients as well as the ring
and runs its tick far less often, and the reason has not been
measured.** Naming the next measurement rather than a cause: instrument
the epoll loop's per-iteration event count and the branch that skips
the tick, then re-read the gauge — the same shape of question the
rewrite-finish arc answered with arm-level timing.

That matters beyond curiosity because the tick drives everysec flushes,
auto-rewrite checks, and TTL eviction. A 440 ms tick interval does not
break the everysec contract (the fsync is submitted by the lane, not by
the tick), but it does mean housekeeping decisions are made at ~2 Hz
under saturation on this reactor.

## Instrument lessons

Two, both mine, both already written down where I did not look first.

- **The first run produced no numbers at all** — every probe died with
  `Connection reset by peer` and the gate printed
  `median p999us= reactor_gap_us=` with empty values, then reported
  FAIL on all four bars. An empty measurement that renders as a failing
  measurement is precisely the failure mode
  `perf-decomposition-vs-polish` §1 warns about. The verdict lines
  looked like data.
- **The cause was a missing `TMPDIR`.** `bench/tailgate.sh` honours
  `${TMPDIR:-/tmp}`, and `/tmp` on this box is a 32 GB tmpfs; a 60 s
  firehose fills it, and the server dies. This exact trap is documented
  in `FINDING-2026-08-09-aof-offload-s1-and-the-rewrite-seat.md`
  ("loses `TMPDIR=captmp` and tailgate lands on the 32 GB tmpfs"). Read
  the prior finding for a gate before running it, not after it fails.
  Isolating it took two rounds of A/B (probe alone on both reactors,
  then probe under load on both reactors) that all passed at 8 s —
  because the tmpfs only fills at 60 s.
