# The zadd pause: five dead suspects, one unbounded loop

The >3 s stall that REFUSED two perfgate runs (and coin-flipped every
zadd angle since) is closed. The hunt, in elimination order — every
suspect killed by a measurement, none by argument:

| # | suspect | killed by |
|---|---|---|
| 1 | giant realloc (alloc+copy+free, no mremap) | timing probe: **zero** reallocs ≥ 20 ms across 10 observed pauses |
| 2 | kernel-side block (mmap_lock, zero-fill) | stall-moment kernel stacks: every reactor thread R, no wchan |
| 3 | SYN drops / accept backlog overflow | ListenDrops/ListenOverflows = 0 across a 6-gap run |
| 4 | missed wake (parked shard) | second stall snapshot: all 8 reactors Running |
| 5 | deferred SQE submission | the loop submits every iteration; owner enters every ~3 ms |

The per-shard loop heartbeat then localized it in one run: **shard 4 —
the zadd hot-key owner — spent up to 1.6 s inside a single loop
iteration** (2–21 iterations/s during episodes; every other shard
≤ 50 ms), with tiny CQE batches — the time was in `drain_inbound`'s
`while pop()` over a peer ring that seven forwarders refill exactly as
fast as it drains. The loop never exits; the owner's *own* direct
clients — accepts, fresh-conn recvs — starve behind it. The 0.9–2.4 s
"pause" was never a pause: throughput continued the whole time
(forwarders talk through rings), only new connections to the owner
waited.

## Fix (`d5312749`)

`DRAIN_SRC_BUDGET = 2048` messages per source per call. Envelopes are
not split (a batch may overshoot); an exhausted source's dirty bit
goes BACK (a bit lost here strands the ring tail until that peer's
next send — forever, if the storm just ended); per-source budgeting
gives fairness without rotation state.

Verified, two storm runs: **gaps 2–6 → 0; worst iteration
1639 ms → 50–66 ms (the park-timeout floor); ZADD 3.52 → 3.47/3.42 M/s
(inside the run band).** kevy-rt + workspace 205 suites green.

Operational dividend: the perfgate zadd coin-flip (REFUSED on 'INFO
stats unreadable') dies with it — median-of-N gating becomes
practical.

## Lessons

- The pause was chased at the wrong layer three times because the
  *symptom* (new-conn latency) reads as "server stalled" while the
  server was at full throughput. The heartbeat probe (per-iteration
  wall clock, per shard) is the tool that ends such hunts; it now has
  a place in the standard kit.
- `cargo check` returning in 0.07 s after a source touch is a cached
  verdict, not a build — the box caught a field-name error the local
  "check" never compiled. Real builds before shipping diffs to the
  box.
