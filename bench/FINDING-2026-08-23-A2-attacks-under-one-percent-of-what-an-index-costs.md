# FINDING 2026-08-23 — A2 attacks under 1% of what an index actually costs

**Status**: A2 is **not** implemented, and should not be until the 669 bytes
below are decomposed. The Pre-Phase-B gate stopped it before any code.

## What A2 proposed

Store an index entry against a **dense row id** instead of the row's key
bytes. The arithmetic in
`bench/FINDING-2026-08-23-a-rows-fixed-cost-is-independent-of-its-columns.md`
put it at **24.6%** of the store (3,699 → 2,265 KB per CSV-MB), the largest
single remaining item on the memory axis.

That number was never measured. The methodology's Pre-Phase-B gate requires
the attack target to be a double-digit share of the total *before* Phase B
starts, on the grounds that an estimate built from an op-cost table is
hand-waving until a measurement agrees.

## The measurement

500,000 seven-column rows with a 400-byte column, RSS per row from outside
the process, one variable at a time — whether an index is declared, and how
long the row key is:

| | no index | with index | the index costs | share of total |
|---|---:|---:|---:|---:|
| short key (`row:N`, ~10 B) | 1,663 | 2,332 | **669 B/row** | **28.7%** |
| long key (`row:tenant-acme-production-cluster-N`, ~36 B) | 1,722 | 2,509 | **787 B/row** | 31.4% |

The table declares two indexes (a `sku` range and a `by_dept_ts` orderpath).

**The index term passes the gate: 28.7% is worth attacking.**

**A2 does not.** Growing the key by 26 bytes grows the index by 118 B/row —
about 59 bytes per index per row, roughly the key growth carried twice. So
the key bytes *are* in there, proportionally. But at the short key that
production shapes actually use, the key is ~10 bytes, and a dense row id is
8: replacing it saves **about 2 bytes per index per row**, against 669 bytes
the index occupies.

**A2 removes under 1% of the term it was aimed at, not 24.6%.**

## Where the 669 bytes are instead

Not measured here, and that is the next step rather than a guess. The
candidates visible in the source are the per-entry structural overhead
(`ENTRY_OVERHEAD = 48` in `crates/kevy-index/src/segment.rs`), the segment's
own node structure, the `IndexValue`, and the derived composite key an
`ORDERPATH` builds per row. Two indexes at 669 bytes is ~334 bytes each,
against 48 declared per entry — so most of it is somewhere the memory formula
does not name.

## Why this is the fifth time

This session predicted and was refuted five times: A1's memory at −24%
(measured −5.1%), A1's load cost at −8.5% (measured −0.03%), the index
backfill explaining the sign difference (it made it worse), packing breaking
tiering (it demoted more than the control), and now this. Every one was an
argument from a cost table.

The difference here is that the gate ran **before** the code. That is the
whole value of the Pre-Phase-B rule, and it is the first time this session it
has been honoured rather than learned from afterwards.

## What N4 becomes

Not "implement A2". **Decompose the 669 bytes an index costs per row**, then
attack whichever term that measurement names. A2 may still be part of the
answer for deployments with long keys — 787 vs 669 says long keys are real —
but it is not the 24.6% item, and nothing should be built on the belief that
it is.
