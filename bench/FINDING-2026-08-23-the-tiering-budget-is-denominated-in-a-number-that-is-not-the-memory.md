# FINDING 2026-08-23 — the tiering budget is denominated in a number that is off by 2–3×, and the packed row makes it stop demoting entirely

**Status**: OPEN, and it is the blocker on turning `packed-rows` on by
default (v5.4.1 N3, ahead of N2).

## The measurement

500,000 seven-column rows with a 400-byte column, an index declared,
`appendfsync everysec`, tiering budget **512 MB**, one binary one flag apart:

| | `used_memory` | RSS | RSS ÷ accounted | demotions |
|---|---:|---:|---:|---:|
| packed off | 442,555,232 | 896,667,648 | **2.03×** | **142,552** |
| packed **on** | **288,726,230** | 865,800,192 | **3.00×** | **0** |

The demotion gate is `self.used_memory <= effective_target(t)`
(`crates/kevy-store/src/tier_demote.rs:126`).

**The general arm works exactly as designed**: it demoted 142,552 keys and
stopped at 442,555,232 against a target of 442,564,588 — inside it by nine
thousand bytes out of four hundred million. The mechanism is not broken.

**The packed arm never demoted once.** Its accounted total, 288 MB, sits far
below its 456 MB target, so the store believes it has 168 MB of headroom. The
process is holding 866 MB.

## What it means

An operator who sets a 512 MB tiering budget gets **866 MB resident** with
packing on, and **897 MB** with it off. Neither is 512 MB, and the number the
engine steers by is 2–3× away from the number the operator is watching.

The packed row does not cause this; it **exposes** it. The gap already
existed at 2.03×, and packing widened it to 3.00× because the packed form's
weight is honest about itself — `heap_bytes` is the buffer plus the inner
struct — while the general hash's weight under-charges by roughly 344 bytes a
row (`HASH_SLOT_BYTES = 32` against a real 49-byte slot, with the `ArcInner`
and the map struct uncharged; recorded in
`bench/FINDING-2026-08-23-a-rows-fixed-cost-is-independent-of-its-columns.md`).

So the more accurate a value's accounting becomes, the *less* memory tiering
reclaims. That is the wrong incentive to have standing in a store.

The remaining term is what no value weight charges: the keyspace slot, the
`Entry`, allocator rounding, the AOF buffers, the index segments, and the
process's own image. At 500k rows the earlier per-row work put the first
three at ~410 B/row, which is ~205 MB here — most of the 454 MB difference in
the general arm.

## Why this blocks the default

`packed-rows` is off today for three reasons; two are now refuted
(`bench/FINDING-2026-08-23-the-gap-opens-when-the-rows-are-read.md`). This is
the third and it stands: turning packing on by default would take a
deployment that was demoting 142,552 keys to hold its budget and stop it
demoting at all. That is a visible regression for every tiered deployment,
and it is not the representation's fault.

## Correction — two obvious ways out are already refuted, by this repository

Both of the exits I reached for first have been measured here and closed.
Recording that before anyone re-reaches for them.

**"Steer by RSS" is the wrong direction.** The 2–3× is not the store holding
memory it could release. It is glibc's `brk` arena, which only shrinks from
the top, so a freed chunk under a live one is a page the OS never gets back
(`crates/kevy-alloc/src/lib.rs:5-13`;
`bench/PERF-FINDING-2026-07-25-b6-rss-glibc-fragmentation.md` measured
`malloc_trim(0)` and `MALLOC_ARENA_MAX=2` and found both no-ops). Steering by
RSS would make the store demote harder chasing a figure demotion cannot move:
the logical data leaves, the fragmentation stays.

**"Switch to `kevy-alloc`" is refuted too.** It was written for exactly this
shape — per-shard, mmap-backed, header-free — and on this workload it costs
**8.5–15% more** memory than glibc, not less
(`bench/FINDING-2026-08-22-kevy-alloc-costs-memory-on-rds.md`, CLOSED, with
the order excluded and the direction reproduced to 0.1 pp).

That finding also recorded the same phenomenon at a different scale: tiered
mode holding `used_memory` at 0.42 GiB while RSS sat at 3.03 GiB — **7.2×**.
The 2.03× and 3.00× above are that, reproduced.

## Correction — "packing makes tiering worse" is not what this measured

The section above asserted that, and the RSS column refutes it — a column
that was in the same table:

| | demotions | RSS |
|---|---:|---:|
| packed off | 142,552 | 896,667,648 |
| packed **on** | **0** | **865,800,192** — **3.4 % lower** |

The packed arm kept every row in RAM and still used less memory than the arm
that spilled 142,552 keys. At this size, demoting nothing was not a
regression; it was the store correctly observing it had room.

I nearly wrote a fix for a defect I had asserted rather than measured.

## What is actually established, and what is not

**Established.** The figure the demotion gate steers by sits 2–3× from the
process's resident memory, and packing widens that ratio because the packed
form's weight is honest about itself while the general hash's under-charges
by roughly 344 bytes a row. The general arm honours its target to nine
thousand bytes in four hundred million; the mechanism is not broken.

**Established.** The fragmentation term has no known recovery. Both exits
were measured here and closed (above).

**Not established.** That any of this harms anyone. With packing on the tier
never engages, so a dataset that genuinely exceeds RAM would not spill and
the budget would stop bounding anything — but half a million rows fit in both
arms, so this probe cannot see that case. **The experiment that can is one
where the data does not fit, and it has not been run.**

## The experiment ran. The hypothesis is refuted.

Three million rows against the same 512 MB budget — data that genuinely does
not fit:

| | demotions | vlog | evicted | DBSIZE | `HGET row:1 name` |
|---|---:|---:|---:|---:|---|
| packed off | 2,994,126 | 412,582,475 | 0 | 3,000,000 | `user1` |
| packed **on** | **2,998,956** | 391,633,468 | **0** | 3,000,000 | `user1` |

**The packed arm spilled more than the control**, by 4,830 keys. Nothing was
evicted, nothing was lost, neither process was killed. All three bad outcomes
this experiment was designed to distinguish — evict instead of spill, grow
until killed, never spill — happened in neither arm.

So the zero demotions at half a million rows was **scale, not a defect**: at
that size the store had room and correctly said so; at three million it does
not, and it engages exactly as it should.

**There is nothing here to fix.** The premise this whole item was written on
— that packing switches tiering off — is wrong.

## What survives

- The accounted figure still sits far from resident memory (5.68× and 5.95×
  here, 2.03× and 3.00× at the smaller size). That is the glibc arena, it has
  no known recovery, and both exits are closed. It is a **property to
  document**, not a defect to fix.
- `tier_effective_target` reaches **0** at this size — the index and stub
  floors consume the whole budget — so the store is permanently "over target"
  and relies on the backoff to avoid re-walking the sample window forever.
  That mechanism exists and works, but a target of zero is worth knowing
  about when sizing a budget.
- **`packed-rows` is no longer blocked by tiering.** The last of the three
  reasons for keeping it off is gone.

## The shape of my own error, since it is the fifth time this session

A plausible mechanism (accounting gets accurate → figure shrinks → gate stops
firing) plus a suggestive observation (zero demotions) is not evidence. Four
candidates for the packed row's sign difference had exactly this shape and
all four were refuted. This is the fifth. The only thing that went right is
that the experiment ran before the fix was written.
