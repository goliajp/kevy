# The collection-write residual, decomposed: the tax has two layers and one of them is load-bearing

The per-word finding's stated next step was to profile whether the
remaining hset tax "is still allocator self-time or has moved". Ran at
`c023ff8a` (per-word state, no hot-slot) on lx64, `perf record -F 499`
against the perfgate legacy topology under sustained hset and zadd,
ON (`--features kevy-alloc`) vs OFF interleaved. Symbols required
`CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_DEBUG=true` —
the workspace profile carries `strip = true`, which also defeats
`DEBUG=true` alone.

## Answer 1 — the named alloc path is still above glibc, but only by ~3 pp

| self-time | OFF (glibc) | ON (kevy-alloc) |
|---|---:|---:|
| hset: allocator symbols | malloc 7.7 + cfree 2.7 = **10.4 %** | Heap::alloc 7.4 + dealloc 3.6 + pop_slot 2.5 = **13.5 %** |
| zadd: allocator symbols | **11.0 %** | **14.0 %** |

Three points of self-time do not explain a −12 to −17 % throughput
deficit. The rest is not in the named allocator symbols.

## Answer 2 — the bigger delta hides in tick-adjacent code, and LTO will not name it

`drain_replica_inbox`'s symbol range — a function whose body is one
early `return` on these standalone servers — inflates from 4.5 %/4.8 %
(OFF) to **13.7 %/16.5 %** (ON): a +9–12 pp delta, the largest between
the builds. Under `lto = "fat", codegen-units = 1` the samples landing
in that range belong to inlined neighbours of the reactor tick, not to
the function. The one ON-only tick component is
`alloc_reclaim_tick()` → `kevy_alloc::thread_reclaim()`, `#[inline]`,
called every shard tick — and it is the mechanism M3's win rests on
(its own comment: measured unwired, resident was 2.39× against glibc's
2.40×, "the design's whole point, absent").

Symbol-granularity attribution cannot split this further. Pricing it
required an experiment, which produced Answer 3 instead of a price.

## Answer 3 — the reclaim tick cannot be turned off to price it: it is load-bearing

A research build with `thread_reclaim()` stubbed out (NR) was run as
the perfgate candidate against OFF. **Twice — once on a dirty checkout
and once after sweeping 289 residual aof files from it — the gate
refused at the same place**: by the zadd angle, `INFO` stopped
answering ("is a shard wedged?"). The legacy topology runs every angle
against one server instance, so by zadd the NR heap has churned through
~300 M+ ops with no page-return at all. A fresh NR server under 60 s of
sustained zadd is stable (RSS flat at 604 MB, PONG throughout) — the
degradation needs the accumulated state, not the burst.

So "no reclaim" is not a configuration; it is a time bomb. The per-tick
reclaim is not merely M3's mechanism — without it the allocator
degrades until the shard stops serving. Any pricing (and any tax
reduction) has to come from **pacing** — a bounded reclaim budget per
tick, or every-N-ticks batching — which is therefore the next design
candidate, ahead of any further alloc-path polish.

## Answer 4 — the tax is a steady-state phenomenon

A blunt 1.8-second three-way (8 M requests) shows OFF ≈ ON ≈ NR at
~4.56 M rps: in a burst the tax does not exist. It accrues with churn —
consistent with reclaim work and span-state growth, inconsistent with a
constant per-op penalty. Short benchmarks will keep failing to see it;
perfgate's counter-based steady window is the only instrument here that
does.

## Instrument notes (four traps this decomposition paid for)

- `pgrep -f 'port N' | head -1` selects the **sudo wrapper** (lower
  pid, same argv substring): the first profile recorded a sleeping
  process, and `kill $srv` orphaned the real servers.
- The workspace's `strip = true` defeats `CARGO_PROFILE_RELEASE_DEBUG`
  alone; `CARGO_PROFILE_RELEASE_STRIP=none` is also required or every
  kevy frame is a bare address.
- `$(redis-benchmark … -q | tail -1)` captures empty; capture the whole
  output and grep the rate out of it.
- perfgate's servers run with `dir=.`, so the box checkout root had
  accumulated 289 aof/premigration files; each instance booted through
  a re-shard/backup of them. Swept; the wedge reproduced anyway, which
  is what cleared the residue as a suspect.
