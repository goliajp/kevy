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
