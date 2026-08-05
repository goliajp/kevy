# A windowed index lost rows to a stale tombstone, and both audits missed it

Found while answering R6's open question (*does a backup include the
cold tier?* — it does; see the last section). The backup answer took ten
minutes. This took the rest.

## The defect

Declare a windowed table, write rows past the window, and a handful of
them become **unreachable through the index while still present in the
keyspace**.

```
TABLE.DECLARE ev PREFIX ev: PK id
    COLUMN id str COLUMN ts i64 COLUMN kind str
    INDEX ts range VALUES kind
    WINDOW ts SPAN 2000 BUCKET 500
HSET ev:{i} id {i} ts {i} kind …     for i in 0..20000
```

| | |
|---|---|
| `DBSIZE` | **20000** |
| `IDX.COUNT ev.ts RANGE 0 19999` | **19981** |
| rows the index cannot find | **19** |

The rows are *there*: `HGETALL ev:15604` returns the full row, `ts`
included. The index simply does not hold it, hot or cold, and no query
against `ts` will ever return it again.

**Reproduces on `develop`** (21 lost on the same script, vs 19 on this
branch) — a **pre-existing defect on released behaviour**, not something
this branch introduced. The count varies run to run, which first read
like a race; it is not. The cause below is deterministic, and what
varies is how many times a bloom filter guesses wrong.

Where they go missing, from a per-bucket count sweep:

```
buckets short: (9000,499) (11500,499) (13500,498) (14000,499)
               (15000,499) (15500,496) (16000,498) (16500,497) (17000,496)
```

Scattered through the **cold** side; the hot window (17500-19999) is
complete.

## The root cause, measured rather than reasoned

The obvious suspect was the seal itself: `slide()` builds the segment
from the below-bound prefix and *then* removes that prefix from the
tree, so anything arriving between the two steps would be dropped
without being written. A probe (`KEVY_PROBE_SLIDE=1`) comparing each
segment's record count against the batch actually removed says
otherwise:

```
24 slides, 0 mismatches — sealed == split_off, every time
```

So nothing is lost at the seam. The same probe carries the tombstone
count, and that is where the number was:

```
PROBE slide ev.ts sealed=503 split_off=503 tombs=19 ok
rows the index cannot find:                       19
```

**Every row in this workload is written exactly once**, so no row can
legitimately own a tombstone — a tombstone exists to shadow a *stale*
cold entry, and nothing here is stale. All 19 are bloom false
positives, and they are the loss:

1. `on_row_write(k)` consults a bloom (`ColdBloom`) before spending a
   tombstone. On a false positive it tombstones `k`, which has no cold
   entry at all — harmless, the comment says, *"a bloom false positive
   spends one stray set entry"*.
2. `k` later slides into a segment for the first time.
3. The tombstone was a **flat `HashSet<row>`** and is never cleared, so
   it now shadows the live entry `k` was just given. Permanently.

The same defect hits a legitimate tombstone too, and harder: a row that
is cold, rewritten (tombstone recorded, correctly), and then slid again
has its **new** entry hidden by the shadow meant for the old one.

`text.rs`, the same crate's text-window path, states the invariant this
one was missing: *"A tombstone is exact, not approximate"* — it keys
tombstones by `(row, segment seq)`.

## The engine's fix: a shadow reaches backwards only

`tombs: HashSet<Vec<u8>>` → `HashMap<Vec<u8>, u64>`, the value being
the sequence number the shadow reaches. A cold entry is hidden only
when its own segment was sealed **before** that number:

```rust
fn shadowed(&self, row: &[u8], seq: u64) -> bool {
    self.tombs.get(row).is_some_and(|&reach| seq < reach)
}
```

`on_row_write` records the current sequence, so anything the row is
given *afterwards* is sealed above the shadow and stays visible. One
`u64` per tombstone; no extra work on the read path beyond a comparison
it was already making.

Measured on the identical workload after the change:

| | before | after |
|---|---|---|
| `IDX.COUNT` over the full range | 19 981 / 20 000 | **20 000 / 20 000** |
| `TABLE.VERIFY` `missing` | 0 (blind) / 17 500 (noise) | **0** |
| `drift`, `duplicates` | 0 | **0** |
| tombstones still recorded | 19 | 17 — still spent, now harmless |

Pinned by two tests in `kevy-window` that **fail on `develop`**: a
shadow must not reach forward to a later segment, and a shadow spent
before the entry existed must hide nothing.

## Why nothing caught it

`TABLE.VERIFY` exists precisely to falsify "a write-maintained index
never drifts". On this table it reports:

| | `missing` |
|---|---|
| `develop` | **17500** |
| this branch | **0** |

Both are wrong, in opposite directions, and the second one is mine.
Neither would have found the defect above; the corrected audit does.

* **`develop` reports every windowed-out row as missing.** 17500 =
  20000 − 2500 (the hot window). Technically the 21 lost rows are inside
  that number; practically the signal is unusable — an operator reading
  it learns nothing and stops reading it.
* **This branch reports zero.** The windowed-row exemption I added
  earlier today removes the noise by exempting any row whose indexed
  value sits below the hot boundary. That is an exemption **by
  position**, and the 19 genuinely-lost rows sit below the boundary too.
  So the fix for the noise swallowed the signal.

This is R4a's tenth lesson — *what cannot be verified will drift* —
landing on the verification surface itself, the same day it was written.
A position-based exemption does not verify anything; it assumes.

## The audit's fix: exempt by evidence, and it costs one number

A row below the boundary is legitimately absent from the hot index **if
and only if it is actually in a cold segment**. Checking that per row
would mean a cold lookup per candidate, and the cold handle does not
outlive the segment scope anyway.

It does not need to be per row. The identity is:

> **missing = (rows below the boundary that should be indexed) − (live
> cold entries)**

Both sides are already available: the classifier counts the first while
walking rows it must walk regardless, and `WindowDir::cold_count` gives
the second as whole-segment arithmetic (no walk) whenever no tombstones
exist. One extra `u64` leaves the segment scope.

Checked against the live instance before writing any code:

```
hot entries          2 500
IDX.COUNT (hot+cold) 19 981   ⇒ live cold entries = 17 481
rows below boundary  17 500   (20 000 − 2 500)
17 500 − 17 481    = 19       = exactly the rows the index cannot find
```

The arithmetic reproduces the defect's own count. That is the check
`TABLE.VERIFY` should have been making.

## R6's actual question, answered

**Does a backup include the cold tier?** The tier lives in
`<data_dir>/tier/<shard>/vlog-*.dat` — a subdirectory — and
`kevy-cli backup`'s `pack()` skips subdirectories with the comment
*"skip subdirs (kevy data_dir is flat anyway)"*, which stopped being
true when tiering landed.

**It is nevertheless correct, and the reason is worth stating rather
than assuming.** Measured end to end: 20 000 × 4 KiB against a 64 MB
budget (demotion engaged — `used_memory` 60.4 MB), `SAVE`, back up,
restore into a fresh directory, boot:

| | |
|---|---|
| files backed up | 5 (both AOFs, both snapshots, `shards.meta`) — no `tier/` |
| restored `DBSIZE` | **20000** |
| `STRLEN` of demoted keys | **4096** each |

A snapshot **materialises** cold values rather than referencing them, so
the backup carries the data even though it skips the tier directory. The
tier is derived spill; the backup is complete without it.

Two things that follow, neither of them a bug today:

* The comment in `pack()` is false and should say *why* subdirectories
  are skipped safely, so the next person to add a subdirectory of
  **truth** does not inherit a silent hole.
* A backup taken with **no snapshot and no AOF** (a memory-only
  deployment has no data dir at all, so this is only reachable
  mid-configuration) has nothing to carry. Out of scope here, named so
  it is not mistaken for tested.
