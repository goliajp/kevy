# FINDING 2026-08-12 — the poll reactor drained its own AOF lane, ten times a second

Follow-on to `FINDING-2026-08-12-tailgate-epoll-observation.md`, which
ended with a question rather than a cause: **what does one iteration of
the epoll loop occasionally spend half a second doing?**

Answer: it settles the AOF writer lane — the one S3 added so the
reactor would stop waiting on writes — because a policy that never
changed was treated as a pending switch on every tick.

## The chain

`KevyCommands::live_runtime_config` reports the live `appendfsync`
every tick, always as `Some(policy)`. `apply_live_persist_knobs` took
that at face value:

```rust
if let Some(f) = live.appendfsync && self.aof.is_some() {
    self.pending_fsync_policy = Some(f);   // every tick, unconditionally
}
self.try_apply_fsync_policy();
```

and the switch protocol — correct in itself, from the
fsync-policy-switch-drain fix — settles the offload driver before
applying, so that a policy change can never interleave with writes
already in flight. On the poll reactors that settle is a busy-wait:

```rust
while self.aof_lane.enabled && !self.aof_lane.appends_drained() {
    self.epoll_aof_reap();
    std::thread::yield_now();
}
```

So at 10 Hz, on every shard, the reactor stopped and spun until the
writer lane was empty. Under the firehose that is most of a second.
The ring never paid it: without a lane its settle returns immediately,
which is exactly why the same runs showed a 39 ms worst gap on uring
against epoll's 465 ms.

## How the seat was named

Not by reading — the mechanism above is obvious *once you know where to
look*, and three of us (the gauge, the client, and the source) had
been looking elsewhere for a day.

1. **The gauge was read wrong first.** `reactor_tick_gap_max_us` is
   `fetch_max`'d and never reset, so it answers "how bad was the worst
   one", never "how often". A first draft read 440 ms as a cadence and
   claimed housekeeping ran at ~2 Hz. Withdrawn.
2. **`reactor_ticks_total`** (this branch) turns the gauge into a pair.
   Calibration: an idle 4-shard server reports `tick_hz = 40.0` = 4 × 10 Hz
   exactly. Measured under load: epoll held **38.0 Hz** on the mixed
   cell and **32.1 Hz** on the firehose. The cadence was healthy; the
   withdrawn claim was off by more than an order of magnitude.
3. **The real signal was a rare single-iteration stall**, and it showed
   from both sides at once — the reactor's worst gap and the client's
   worst RTT agreed to within ~20% (448/529 ms mixed, 911/996 ms
   firehose). Too rare for a p99.9 bar: at 1 kHz over 30 s, p99.9 is
   the worst ~30 samples and this is one.
4. **`KEVY_DEBUG_SLOW_ITER_MS`** (this branch) prints a phase
   breakdown for any iteration over a threshold. First run put the
   whole stall in `tick` with **zero events processed**; splitting the
   tick body put **884 ms of an 891 ms iteration** inside
   `apply_live_runtime_config`. One call, one suspect.

## Result

Box, 4 threads, NVMe, same build, 30–45 s windows:

| case | metric | before | after |
|---|---|---:|---:|
| epoll mixed | reactor gap max | 465 ms | **42 ms** |
| epoll mixed | client max RTT | 256 ms | **13 ms** |
| epoll mixed | tick_hz | 39.1 | 39.8 |
| epoll firehose | reactor gap max | 790 ms | **34–42 ms** |
| epoll firehose | client max RTT | 862 ms | **14–16 ms** |
| epoll firehose | p99.9 | 123 ms | **10 ms** |
| epoll firehose | tick_hz | 33.6 | 37.8–38.0 |
| epoll firehose | iterations > 100 ms | 97–106 | **0** |

**tailgate now PASSES on the poll reactor**, where it was over the bar
on three of four numbers the same morning:

| cell | metric | epoll before | epoll after | uring (same day) |
|---|---|---:|---:|---:|
| mixed | p99.9 | 4.68 ms | **0.62 ms** | 8.15 ms |
| mixed | gap | 440 ms ✗ | **48.0 ms** | 44.96 ms |
| firehose | p99.9 | 117 ms ✗ | **10.4 ms** | 12.58 ms |
| firehose | gap | 774 ms ✗ | **69.1 ms** | 55.91 ms |

crashgate PASS (the gate that owns `always` semantics, which is the
path this touches). The deferral half of the switch protocol is
untouched: a pending switch that cannot apply yet stays pending for the
next tick, and the uring drain check is unchanged.

## Notes

- **This never reached a user.** The writer lane is new in 5.1 (S3) and
  so is the settle-before-switch discipline, so 5.0 had neither the
  lane to drain nor the tick that drained it. What the fix changes is
  how much of S3's benefit actually arrives.
- **Recommendation, not applied**: with epoll inside the bars, the
  tailgate epoll cell has earned a bar of its own. Adding one is a
  scope decision (it doubles the gate's wall-clock), so it is written
  here rather than committed.
- **Third instrument lesson of this arc**, and the same shape as the
  other two: the residual-slow-iteration count printed by my own
  measurement script said "5" for every case, including runs whose
  server logs contained nothing but their 5 startup lines. The number
  was the file's line count, not a match count. The probe's own numbers
  — independent of that script — are what the tables above report. A
  measurement device's failure looks exactly like data; this arc
  produced three separate instances of that in one day (an empty gate
  reading as four failing bars, a high-water gauge reading as a rate,
  and now a line count reading as a hit count).
