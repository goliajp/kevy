# FINDING 2026-08-23 — declaring before loading and declaring after are not the same memory

**Status**: measured on this machine (macOS), one variable, 200,000 rows of
the benchmark's exact shape. The Linux half is not measured yet and the
numbers below must not be assumed to transfer — the two platforms' allocators
differ in exactly the behaviour this measures.

## The measurement

One binary, `KEVY_PACKED_ROWS` on or off, `--no-aof`, RSS read from outside
the process, per row over the server's own baseline. "Peak" is taken when the
load finishes; "settled" after the last key in the range has converted — not
the first, because the backfill runs in batches and watching an early key
reports settled while both representations are still live for everything
else.

| order | packed | peak B/row | settled B/row | `MEMORY USAGE` |
|---|---:|---:|---:|---:|
| declare, then load | no | 1,609 | 1,609 | 1,200 |
| declare, then load | **yes** | **991** | **991** | 578 |
| load, then declare | no | 1,601 | 1,602 | 1,200 |
| load, then declare | **yes** | 1,606 | **2,481** | 578 |

- Declaring first: **−38.4%**, and the per-row accounting and RSS agree on
  the direction and roughly on the size.
- Declaring after: **+55%**. Worse than not packing at all, while the
  engine's own accounting for the same row says 578 bytes either way.

## Why

Declaring first means every row is built packed by the write hook and the
general form is never allocated. Declaring after means every row is built in
the general form, and the backfill then allocates a packed buffer beside the
table it replaces and frees the table. The peak is unchanged — 1,606 against
1,601, as it must be — and what the settled figure says is that **the freed
tables do not come back to the process**, while the new buffers are counted.

That is an allocator property, not an engine one, and it is why the platform
caveat at the top matters. This machine's allocator kept both. glibc's arena
behaves differently, and the box's first pass on the same order shows RSS
*improving* (5,504 → 4,760 with no AOF), so the Linux answer is a different
number and possibly a different sign.

## What it does not license

It does not license "declare first and the problem goes away", because the
deployment shape that matters most — adopting the feature on a live keyspace
— is exactly the load-then-declare order. Nor does it license re-ordering
`bench/pgcompare.py`: the benchmark loads then declares, which is the honest
shape for adoption, and changing it to flatter the feature would hide this.

## What it changes

Two things, both for the owner rather than decided here:

1. **The default.** With `packed-rows` on by default, a server that declares
   a table over rows it already holds can end up **using more memory**, on at
   least one supported platform, for as long as its allocator holds the freed
   tables. Off by default makes that an opt-in with a documented shape.
2. **The backfill's mechanism.** Converting in place is the cheapest thing
   that is correct, and it is allocation-neutral only if the allocator
   returns what it frees. A conversion that instead happened at a natural
   rewrite point would not have this property, and is not designed here.

## Next measurement

The same two orders on the box, on Linux, at the benchmark's scale. Until
that exists, the only statement supported for Linux is the one the three
interleaved passes produce for the load-then-declare order, and that order's
number is a floor rather than the representation's own effect.
