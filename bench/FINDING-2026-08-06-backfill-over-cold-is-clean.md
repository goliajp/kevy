# Two negative results: backfill and TABLE.REPLACE over cold data are clean

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

## Second surface, same answer: `TABLE.REPLACE` on a windowed table

`TABLE.REPLACE` is the migration verb — drop and redeclare, rebuilding
every compiled index from the rows. On a **windowed** table it also has
to deal with cold segments belonging to the index it is dropping, which
is the structure this session's tombstone defect lived in, so it was
the next reasonable suspect.

8 000 rows past a `SPAN 2000 BUCKET 500` window, sealed into cold
segments, then replaced with a changed spec (a `VALUES` column added,
forcing every compiled index to rebuild):

| | before | after replace | after 3 replaces |
|---|---|---|---|
| reachable through the index | 8000 / 8000 | **8000 / 8000** | **8000 / 8000** |
| `TABLE.VERIFY` missing / drift / duplicates | — | **0 / 0 / 0** | — |
| cold index segment files | 11+ | **1** | **1** |
| segment directory | — | 344 K | **344 K** |

Coverage survives the rebuild, and the **old segments are dropped
rather than orphaned**: three replaces in a row leave the file count
and the directory size byte-identical. A verb that leaked a segment set
per call would have shown as monotonic growth here, and it is the kind
of leak nobody notices until a disk fills.

## Why this is worth a file

The six earlier findings all describe something broken, which makes the
lens look like it always pays. It does not, and the difference matters
when deciding where to point it next: this surface is **already
covered**, so effort belongs elsewhere.

It also closes specific worries rather than a general one. The two
index-visibility defects fixed today (a multi-key delete leaving stale
entries; a stale tombstone hiding cold entries) both lived on the
**maintenance** path. Backfill is the *other* way an index gets its
contents, and `TABLE.REPLACE` is the path that discards and rebuilds
one wholesale — including the cold segments the tombstone defect lived
in. Both were reasonable suspects. Neither shares the blind spot.

**Three of the engine's four ways of populating an index are now
measured**: maintenance (two defects, fixed), backfill (clean), rebuild
(clean). The fourth is replica apply, checked earlier in the session on
the write side.
