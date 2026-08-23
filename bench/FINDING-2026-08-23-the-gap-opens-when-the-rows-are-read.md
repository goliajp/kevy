# FINDING 2026-08-23 — the packed row's saving appears when the rows are read, not when they are written

**Status**: closes the "unexplained" note in `docs/packed-rows.md`. Five
candidates, four refuted, the fifth reproduces across three passes.

## The question

Two measurements of the same order — load two million rows, then declare the
table — disagreed in sign. The full benchmark **saved 13.5 %**; every probe I
built **cost** memory. Four candidates were measured and ruled out:

| candidate | verdict |
|---|---|
| the index backfill's allocations reusing what packing freed | refuted — with an index it is **worse** (+19.6 %) |
| shard count | ruled out — both sides run the box's default 16 |
| scale | refuted — two million rows gives +3.5 %, the figure half a million gives |
| pipelined bulk loading | refuted — one-at-a-time +3.6 %, batched +3.5 % |

One difference was left: `pgcompare` runs four query shapes 5,000 times each
**after** the load. No probe had ever run a single query.

## The measurement

Same probe, load-then-declare, an index declared, 500,000 rows, median of
three, one variable — whether 5,000 `HMGET`s and 5,000 `IDX.QUERY … FIELDS`
run after the load:

| | packed off | packed on | |
|---|---:|---:|---:|
| no queries | 1,848 | 2,077 | **+12.4 %** |
| **queries** | **2,207** | **2,133** | **−3.4 %** |

Bytes of resident memory per row. **The sign flips.** This is the first of
the five candidates to move the direction at all, and it moves it the whole
way.

The asymmetry is the finding:

| | RSS added by the query phase |
|---|---:|
| packed **off** | **+359 B/row** (2,042…2,286 across passes) |
| packed **on** | **+56 B/row** (2,133 every pass, to the byte) |

## What it means

The representation's benefit is **not** collected at write time. At write
time packing costs — a buffer allocated beside the table it replaces, and
what the allocator keeps. It is collected at **read** time: serving a field
out of a general hash walks a table and hands back a `SmallBytes` per field,
while a packed row's values are one contiguous run and its reads allocate
almost nothing. Over 10,000 reads that difference is 300 bytes a row of
retained arena.

So the benchmark and the probes were never in conflict. They were measuring
two halves of the same shape, and only the benchmark ran both.

**A row is written once and read many times**, which is the case the
representation was for and the case no probe had been testing. That the
packed arm lands on 2,133 to the byte on all three passes, while the general
arm scatters over 244 bytes, is the same fact from the other side: reads that
do not allocate do not vary.

## Consequences

1. **`docs/packed-rows.md` had this marked unexplained.** It is explained.
2. **The default is decidable again.** The third reason for keeping
   `packed-rows` off was this unexplained sign; it is closed, and closed by
   the form being *better* under read load rather than worse. The other two
   reasons stand and are separate: the adoption path still costs memory until
   the allocator gives it back, and tiering still goes the wrong way because
   its budget is denominated in `used_memory`.
3. **Every memory measurement of a storage form needs a read phase.** A
   load-only probe measures the half where this representation loses. That
   generalises past A1 and belongs in whatever measures A2.
