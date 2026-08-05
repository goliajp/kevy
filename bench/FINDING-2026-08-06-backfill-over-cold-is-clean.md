# A negative result: index backfill over a mostly-cold prefix is clean

Seven defects came out of one lens today — write on one path, read on
another — so it is worth recording where it found **nothing**. A lens
that only ever produces hits is one nobody is checking honestly.

## The surface

`IDX.CREATE`'s backfill reads every row under a prefix to build the
index. If it mishandled a demoted row, the result would be the defect
this session already fixed twice: rows present in the keyspace,
invisible to every indexed query. `docs/tiering.md` also makes a
specific claim about it:

> Bulk paths never promote: … index backfill … reads cold values
> through a non-promoting peek path. `IDX.CREATE` backfill on a fully
> cold table reads one record per row and puts nothing back.

Both halves are checkable.

## The measurement

6 000 hash rows of ~3 KB against an 8 MB tier budget, left to settle,
then an index declared over the prefix:

| | |
|---|---|
| rows | 6 000 |
| of which cold | **5 002** |
| indexed after backfill | **6 000 / 6 000** |
| `promotions_total` | **0** |
| `peek_preads_total` | 4 964 ≈ one per cold row |

**All three agree.** Coverage is complete, nothing was promoted, and
the read count matches one record per cold row rather than a page or a
whole-value fetch. The documented claim holds as stated.

## Why this is worth a file

The six earlier findings all describe something broken, which makes the
lens look like it always pays. It does not, and the difference matters
when deciding where to point it next: this surface is **already
covered**, so effort belongs elsewhere.

It also closes a specific worry rather than a general one. The two
index-visibility defects fixed today (a multi-key delete leaving stale
entries; a stale tombstone hiding cold entries) both lived on the
**maintenance** path. Backfill is the *other* way an index gets its
contents, and it was reasonable to suspect it shared the blind spot.
It does not.
