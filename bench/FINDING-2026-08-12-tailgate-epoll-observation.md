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
45 ms). Those two facts cannot both be "epoll is slower".

**Correction, measured the same day (see the addendum below).** The
first draft of this section read the 440 ms as a cadence and said
housekeeping "is made at ~2 Hz under saturation". The gauge cannot
support that: `reactor_tick_gap_max_us` is `fetch_max`'d from the tick
and never reset, so it is a **high-water mark** — one late tick and a
chronically starved loop print the same number. The claim was
withdrawn and measured instead.

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

## Addendum — the cadence, measured (2026-08-12)

`reactor_ticks_total` (added on `feature/epoll-tick-cadence`; counted
where the reactor already reports the gap, so no new call site and no
hot-path cost) turns the gauge into a pair. `tail_probe` reads it at
both ends of its window. Calibration: an idle 4-shard server reports
**tick_hz = 40.0**, which is 4 shards × 10 Hz exactly.

Box, same build, same disk, 30 s windows:

| case | tick_hz (ideal 40) | gap max | client max RTT |
|---|---:|---:|---:|
| uring idle | 39.9 | 0.8 ms | 2.2 ms |
| epoll idle | 39.8 | 1.0 ms | 1.6 ms |
| uring mixed | 39.7 | 42 ms | 13.6 ms |
| **epoll mixed** | **38.0** | **448 ms** | **529 ms** |
| uring firehose | 38.6 | 22 ms | 18.0 ms |
| **epoll firehose** | **32.1** | **911 ms** | **996 ms** |

**The cadence is healthy.** epoll runs 38.0 Hz of a nominal 40 on the
mixed cell (95%) and 32.1 Hz on the firehose (80%). Nothing here is
starved at 2 Hz; the withdrawn claim was off by more than an order of
magnitude.

**The real finding is a rare half-to-one-second single-iteration
stall on epoll**, and it is visible from both sides at once: the
reactor's worst gap and the client's worst RTT agree to within ~20%
(448/529 ms on mixed, 911/996 ms on the firehose). uring's equivalents
are 42 ms and 22 ms — an order of magnitude smaller. The bars never
caught it because it is rarer than 1-in-1000: at 1 kHz over 30–60 s,
p99.9 is the worst ~30 samples, and this is one sample.

So the epoll tail question is not "why is the tick slow" but **"what
does one iteration of the poll loop occasionally spend half a second
doing?"** — a single-seat question of the same shape as the
rewrite-finish arc, and the same method applies: name the seat with a
measurement (a slow-iteration breakdown, mirroring the existing
`KEVY_DEBUG_STALL_MS` dump on the ring), never with a guess.

**Lesson, the third instrument one in this document**: a high-water
gauge answers "how bad was the worst one", never "how often". Reading
a rate off a `fetch_max` is the same class of error as reading data off
an empty measurement — and I made both in one arc, on the same gate.
