# `packed-rows` — store a declared row as one allocation

`TABLE.DECLARE` tells the server a table's whole shape. With `packed-rows`
on, rows under that table's prefix stop paying for a per-row hash table and
store their column values in declared order behind a small offset table
instead.

It is **off by default**, and the section on ordering below is why.

## When you need this

A table whose rows have three to twelve columns and whose memory matters.
That is the range where the general hash wastes the most: a hash promoted
out of the inline form asks for the smallest table `kevy-map` will give it,
which is sixteen slots, and it asks for that whether the row has three
columns or twelve.

The signature is that **a row's fixed cost does not depend on how many
columns it has**. Measured, 50,000 rows, RSS per row: 1,699 bytes at three
columns, 1,694 at seven, 1,713 at twelve — while their payloads differ by
91 bytes.

## Core idea

A declared row becomes one allocation:

```
[ncol u16][present bitmap][end_1 u16] … [end_n u16][value bytes …]
```

Column order comes from the declaration, so **no field names are stored** —
they are in the catalog already. An absent column costs one bit. Nothing
about this is visible on the wire: `HGET`, `HSET`, `HDEL`, `HGETALL`,
`HRANDFIELD`, `HSCAN` and the field-TTL verbs answer exactly as they did.

Two things earn the general form back, per row, with no error and no lost
value: a write to a column the table does not declare, and a row whose
values exceed what `u16` offsets can address. A packed row is a size class
and a declaration, not a type.

## Turning it on

```toml
[server]
packed_rows = true
```

or `KEVY_PACKED_ROWS=1`, or at runtime:

```
CONFIG SET packed-rows yes
```

`CONFIG SET` changes the running server, not the file — a restart reads the
file, so a server told to pack at runtime comes back not packing.

Rows that already exist when a table is declared are converted by a
backfill, in bounded batches on the shard tick, so declaring a table over a
live keyspace does not stall it.

## What it costs and what it saves

Measured on the release box, two million rows of a seven-column table with a
400-byte column, three interleaved passes, one binary one flag apart:

| AOF mode | packed off | packed on | |
|---|---:|---:|---:|
| off | 5,504 | 4,760 | **−13.5%** |
| `everysec` | 5,824 | 5,527 | **−5.1%** |
| `always` | 5,882 | 5,137 | **−12.7%** |
| tiering on | 5,221 | 5,460 | **+4.6%** |

KB of resident memory per MB of source CSV. Load throughput is unchanged
(within a tenth of a percent). Point reads and index lookups are unchanged.

**The list page costs about 7% more** (p50 145 → 155 µs). A packed row finds
a column by scanning its declared names — a handful of short slices — which
is the trade for not having a per-row hash table, and a page hydrates one
column across twenty rows.

## The ordering, which matters more than any of the above

Whether a saving appears at all depends on **when the table is declared**.

| order | packed off | packed on | |
|---|---:|---:|---:|
| declare, then load | 1,663 | 1,274 | **−23.4%** |
| load, then declare | 1,663 | 1,721 | **+3.5%** |

Bytes of resident memory per row, 500,000 rows, **no index**. Declaring
first means every row is built packed and the general form is never
allocated. Declaring afterwards means every row is built general, and the
backfill then allocates a packed buffer beside the table it replaces and
frees the table — and **what reaches the process is only what the allocator
gives back**.

The peak is identical either way, which is the point: the representation is
not what differs, the history is.

**Two of the measurements on this page disagree, and the named explanation
for it has been measured and is wrong.** The table above the ordering one is
also the load-then-declare order, and it *saves* 13.5% — while this one costs
3.5%. The obvious suspect was the index: the benchmark's table declares one,
whose backfill allocates heavily right where the packing backfill has just
freed, and an allocator reusing those tables rather than holding them would
account for the difference.

Measured, same probe with an index declared, median of three:

| load-then-declare | packed off | packed on | |
|---|---:|---:|---:|
| no index | 1,663 | 1,721 | +3.4% |
| **with an index** | **1,828** | **2,186** | **+19.6%** |

The index makes it worse, scale gives the same +3.5% at two million rows,
the shard count and AOF setting are identical on both sides, and loading
one-at-a-time instead of in batches changes nothing (+3.6% against +3.5%).

**The answer is the query phase.** The benchmark runs four query shapes 5,000
times each after the load; no probe had ever run one. Adding them, median of
three:

| load-then-declare, with an index | packed off | packed on | |
|---|---:|---:|---:|
| no queries | 1,848 | 2,077 | +12.4% |
| **with queries** | **2,207** | **2,133** | **−3.4%** |

The sign flips, and the asymmetry says why: the query phase adds **359 bytes
a row** to the general form and **56** to the packed one — which lands on
2,133 on every pass, to the byte, while the general arm scatters over 244.

**The saving is collected when rows are read, not when they are written.** A
write into a packed row costs: a buffer allocated beside the table it
replaces, and whatever the allocator keeps. A *read* out of a general hash
walks a table and hands back one `SmallBytes` per field; a packed row's
values are one contiguous run and its reads allocate almost nothing. Over ten
thousand reads that is 300 bytes a row of retained arena.

So a row is written once and read many times, which is the case this form is
for — and the case a load-only measurement never sees.

Read the −23.4% as the representation's own effect. Read every load-first
figure as saying that the backfill path's outcome depends on something this
page has not identified, and that at half a million rows it costs memory in
both configurations tested.

So:

- **Declare the table before loading it** wherever you control the order.
  That is where the −23% lives.
- **Adopting it on a keyspace you already have** is the other row, and it
  can cost memory rather than save it until the allocator returns what the
  backfill freed. How much it returns is a property of the platform, not of
  kevy: the same experiment on macOS settles at +55% instead of +3.5%.

## Trade-offs

- **Tiering.** The demotion budget is denominated in the store's own
  accounting, and packing lowers that. A store with tiering on therefore
  sees itself under budget sooner and keeps more rows resident — which is
  the budget doing what it was told, and is why the tiering row above is the
  one that goes the wrong way. Its writes get much faster for the same
  reason: rows that stay hot do not take the tier path on a write.
- **Undeclared keys keep every representation they have today.** The packed
  form is earned by a declaration and never imposed.
- **The inline form still wins for tiny hashes** and stays in front of this
  one.

## FAQ

**Does the AOF or a snapshot change format?** No. The AOF is a command log,
so a packed row is rebuilt by replaying the same `HSET` frames a 5.3 log
already contains, and a 5.3 binary reads a 5.4 log unchanged. Snapshots
re-emit the same payload shape.

**Does a replica have to match its primary?** Yes, in practice: a failover
onto a replica that stores rows the other way changes the memory profile of
the deployment. That is why the setting is hot-settable.

**Can I tell whether a row is packed?** `MEMORY USAGE` on it, before and
after. There is deliberately no verb that reports the storage form: a client
that branched on it would depend on something it must not.
