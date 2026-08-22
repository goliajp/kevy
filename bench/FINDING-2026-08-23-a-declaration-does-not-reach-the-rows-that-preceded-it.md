# FINDING 2026-08-23 — a table declaration does not reach the rows that came before it

**Status**: OPEN. The defect is real, the box measurement that exposed it is
discarded, and the fix is the next step of v5.4 Axis A.

## What was measured

A1's packed row, measured on the box against the 5.4 baseline, one flag
apart on the same binary, two interleaved passes:

| arm | mode | RSS KB per CSV-MB | load rows/s |
|---|---|---:|---:|
| `KEVY_PACKED_ROWS=0` | everysec | 5,825 | 148,738 |
| `KEVY_PACKED_ROWS=1` | everysec | **5,816** | 148,785 |
| `KEVY_PACKED_ROWS=0` | none | 5,504 | 153,766 |
| `KEVY_PACKED_ROWS=1` | none | **5,505** | 154,068 |

Nine bytes per CSV-MB, in both directions. On this machine the same flag on
the same code moved RSS per row from 1,697 B to 1,091 B — a 35.7% saving.

## Why the two disagree

Not because the flag failed to arrive. The run's own witness, taken before
the benchmark started, on the box's binary:

```
KEVY_PACKED_ROWS=0 → MEMORY USAGE 1440
KEVY_PACKED_ROWS=1 → MEMORY USAGE  543
```

And not because the config file overrode it: `resolve_config` applies TOML,
then environment, then CLI, so the environment wins over the file that
`pgcompare` writes.

The difference is the **order of two statements**. `bench/pgcompare.py`
loads all two million rows and declares the table afterwards
(`bench/pgcompare.py:389-397`) — the rows are written first, the index
builds by backfill. My local measurement declared the table first.

A row is packed by `table_runtime::on_write`, which fires on writes. A row
written before any declaration existed was never offered to it, and
**nothing packs a row that already exists when its table is declared**. Two
million rows sat in the general form for the whole run, and the flag was
free because it did nothing.

## The defect this is a symptom of

It is not a benchmark artefact, and re-ordering the benchmark would hide it.
Three ordinary sequences leave rows unpacked:

1. **`TABLE.DECLARE` over an existing keyspace** — the benchmark's shape,
   and the natural one for adopting the feature on live data.
2. **Restore from a snapshot** — `snapshot_read` installs rows through
   `Store::load_hash`, which bypasses the dispatcher and therefore the write
   hook. AOF replay does *not* have this problem: it goes through
   `replay_dispatch` → `Commands::dispatch` → `on_write`, so an AOF-restored
   server comes back packed. A snapshot-restored one does not.
3. **A row nothing writes again.** Once unpacked, a row stays that way until
   the next write to it, and a read-mostly table has none.

For a representation whose entire purpose is memory, "applies only to rows
written after you declared the table" is most of the population missing.

## What it does not say

It does not say A1's saving is real at scale. That is still unmeasured: the
35.7% is one local run of 50k rows, and the box has yet to see a packed row.
The two claims to keep apart —

- the packed form saves memory — **local single-run evidence only**;
- the packed form reaches the rows — **measured false, on the box**.

The second is what this finding closes. The first is measured after it.

## Fix

Pack existing rows under a declared prefix in bounded batches per tick,
which is how the index backfill already handles the identical problem
(`index_runtime::on_tick` → `advance_backfill`, 2,048 keys per tick, from a
key list collected at declare time). The same trigger covers the snapshot
case, since the catalog is loaded there too.

**Verified after the fact, not assumed** (this sentence was written before
it was checked): a server restarted from a snapshot with the switch on
repacks its rows — `row:5` went 608 bytes → 150 within 100 ms, at two shards
and at eight. Checking it also turned up a separate property worth knowing
before measuring anything across a restart: `CONFIG SET packed-rows` writes
the running config, not the file, so a server told to pack at runtime comes
back **not** packing. The first attempt at this check read as "the backfill
misses the snapshot" for exactly that reason.

## How it was caught

By a witness that had nothing to do with the number being measured: the
run printed `MEMORY USAGE` for one row under each setting before starting.
Without it the reading was "A1 does not work on the box, revert it per RFC
§8 step 5" — a conclusion with a measurement behind it, and wrong.

## Prediction, written before the measurement

Recorded here so the comparison cannot be assembled after the fact.

Per row, measured locally on the benchmark's exact shape — seven columns,
a 400-byte `pad`, declared then packed: **1,168 B → 567 B**, a 601-byte
saving per row.

At two million rows that is 1.20 GB. The `everysec` baseline is 5,824 KB per
CSV-MB across 853.9 MB of CSV, or about 4.74 GiB resident.

| quantity | predicted |
|---|---:|
| RSS after | 3.62 GiB |
| RSS per CSV-MB after | **≈ 4,450** (from 5,824) |
| change | **−24%** |
| vs PostgreSQL's 881 | 5.1× (from 6.6×) |

Two things predicted to move the wrong way:

- **Load throughput down.** The conversion reads the row back and rebuilds a
  buffer once per row, and a bulk load is every row. Locally that cost 8.5%.
- **A transient of the backfill's own.** Collecting the key list allocates a
  `Vec<u8>` per key; at two million keys that is tens of MB the allocator
  will likely not return to the OS. It should be invisible against a
  1.2 GB saving, and it is the whole story in a run where nothing packs —
  which is what the previous run measured (+0.7% to +2% RSS, no packing).

If RSS lands within a few percent of 4,450 the arithmetic in §3 of the A1
RFC holds at scale. If it lands near 5,824 again, something is still
refusing rows and the witness in each result row will say so.

## What the first pass says, and the account that does not close

Pass 1 of three, both arms, one flag apart on one binary. The witness in each
row confirms the form took: `sample_row_bytes` 1,200 against 578.

| mode | packed=0 | packed=1 | |
|---|---:|---:|---:|
| none | 5,504 | 4,760 | −13.5% |
| everysec | 5,824 | 5,522 | −5.2% |
| always | 5,882 | 5,155 | −12.4% |
| tiered | 5,218 | 5,440 | +4.3% |

n=1 per cell, so the spread between modes is not yet a finding. What is
already visible is that **the prediction of −24% is not met**, and the reason
is not that fewer rows packed than expected:

| | |
|---|---:|
| per-row difference, by `MEMORY USAGE` | 622 B |
| × 2,000,000 rows | 1.24 GB |
| RSS actually saved, `none` | 0.61 GB — **52%** of it |
| RSS actually saved, `always` | 0.59 GB — **51%** |
| RSS actually saved, `everysec` | 0.25 GB — **21%** |

The same disagreement appears on this machine at 50k rows and is not
box-specific: `MEMORY USAGE` 1,440 → 543 (897 B saved per row) against an
RSS-per-row of 1,697 → 1,091 (606 B saved). Two thirds, measured from
outside the process.

### Correction — the accounting is not the culprit

The paragraph that stood here blamed the engine's own accounting, saying it
overstated the saving about twofold. **That was wrong**, and the measurement
that settles it is in
`bench/FINDING-2026-08-23-the-order-of-two-statements-decides-the-memory.md`:

| | accounted | RSS per row | difference |
|---|---:|---:|---:|
| general hash | 1,200 | 1,609 | 409 |
| packed row | 578 | 991 | 413 |

Both under-report by the same ~410 bytes — the keyspace slot, the `Entry`,
and allocator rounding, which neither charges to the value. So the *delta*
is accurate: 622 accounted against 618 resident, agreeing to within 1%.

The shortfall on the box is therefore **the order, not the instrument**. The
benchmark loads two million rows in the general form and declares the table
afterwards, so every packed buffer is allocated beside the table it replaces
and the freed tables stay in the allocator's arena. The saving that reaches
RSS is whatever the allocator chooses to return.

`used_memory` still deserves the caution: the tiering target is resolved
against it (`tier_demote.rs:126`), so a representation that lowers the
accounted cost lets more rows stay hot under the same budget. That is the
budget doing what it was told, and it is the likely reason the `tiered` arm
is the one mode where packing does not help. Documented rather than
"fixed" — a byte budget denominated in accounted bytes is not wrong for
becoming easier to satisfy when rows get cheaper.

## The result, three interleaved passes, both arms

One binary, one flag, median of three. The control arm reproduces the 5.4
baseline to within single digits per cell (5,882 / 5,824 / 5,504 / 5,221
against 5,880 / 5,821 / 5,504 / 5,221), so the harness is stable and the
comparison is between the arms rather than between runs.

| mode | packed=0 | packed=1 | RSS | load rows/s | page p50 |
|---|---:|---:|---:|---:|---:|
| none | 5,504 | 4,760 | **−13.5%** | 154,694 → 153,248 | 145 → 155 |
| everysec | 5,824 | 5,527 | **−5.1%** | 148,730 → 148,686 | 149 → 158 |
| always | 5,882 | 5,137 | **−12.7%** | 32,417 → 32,117 | 148 → 155 |
| tiered | 5,221 | 5,460 | **+4.6%** | 144,585 → 144,617 | 164 → 156 |

## Both predictions were wrong, in opposite directions

The prediction recorded above this section said −24% RSS and about −8.5% on
load. Measured:

| | predicted | measured |
|---|---|---|
| RSS, `everysec` | −24% | **−5.1%** |
| load | −8.5% | **−0.03%** |

**The memory saving is a third to a half of what the arithmetic said**, for
the reason the order finding gives: the benchmark loads two million rows in
the general form and declares afterwards, so every packed buffer is taken
beside the table it replaces and only what the allocator returns reaches
RSS. The per-row arithmetic was not wrong about the representation; it was
wrong about how much of it a process gives back.

**The load cost did not appear at all.** The 8.5% came from a single local
run of 50,000 rows, and at two million rows on the box the two arms are
within a tenth of a percent — the conversion is not on the load path at all
in this order, because the backfill runs on the tick after the load, not
during it. That is a prediction refuted by its own measurement, and the
refutation is more useful than the estimate was: **§5b's write-amplification
warning applies to the declare-first order, and this order does not pay it.**

## What moved that was not predicted at all

- **`tiered` is the one mode where packing makes memory worse** (+4.6%), and
  it has a named cause: the demotion gate is `used_memory <=
  effective_target` (`tier_demote.rs:126`). Packing lowers the accounted
  cost, the store sees itself under budget sooner, and fewer rows demote.
  The witness shows it directly — the control arm's sample row is a cold
  stub (96 bytes), the packed arm's is still hot (578).
- **`tiered` writes get much faster** as the other face of the same thing:
  write p50/p99 163/242 → 31/43, because rows that stay hot do not take the
  tier path on write. A byte budget that is easier to satisfy keeps more
  rows in RAM and makes them cheaper to write. Both halves are the budget
  doing what it was told.
- **The list page costs about 7% more** (p50 145 → 155 at `none`, 149 → 158
  at `everysec`). A packed row answers `get_named` by scanning its declared
  names, which is the design's own trade — no per-row hash table, a linear
  scan of a handful of short slices instead — and the page shape hydrates
  one column per row across twenty rows. Small, consistent across modes,
  and the first measured cost of the representation.

## Verdict against RFC §8 step 5

The step says revert the axis if the RSS term does not move. **It moves**, in
three of four modes, by 5–13.5%. It stays.

It does not meet its own prediction, the fourth mode goes the other way, and
the reads cost about 7% more on one shape. All three belong in the release
notes rather than in a footnote, and the default stays off until the owner
decides otherwise.
