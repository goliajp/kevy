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

## Correction — "under 1%" was my own arithmetic error

The paragraph above computed A2's saving as `keylen − 8` per index per row.
That treats the key bytes as the whole of what an entry spends on the key,
and reading `Segment` says otherwise:

```rust
tree:  BTreeSet<(IndexValue, Vec<u8>)>   // the value + a full copy of the key
back:  HashMap<Vec<u8>, IndexValue>      // a second full copy of the key
```

**The key is stored twice**, and each copy is a `Vec<u8>` — 24 bytes of fat
pointer on 64-bit plus its own heap allocation, not just the bytes. So what a
dense row id replaces is `2 × (24 + keylen)`, not `keylen`.

The structural account agrees with the measurement, which is why it is worth
believing: long keys cost 118 B/row more across two indexes, i.e. **59 per
index**, against a predicted `2 × (36 − 10) = 52`.

| key | key-related cost per index per row | share of the ~334 B an index spends |
|---|---:|---:|
| 10 B (short) | 68 B | **20%** |
| 36 B (long) | 120 B | 36% |

At a short key A2 would remove `2 × (24 + 10 − 8) = 52` bytes — **~16% of
what an index costs**, not 1%.

That does not restore the 24.6% claim, which was a share of the whole store,
and it does not remove A2's real precondition: it needs a stable dense
identifier that A1 does not create. The gate's verdict stands — do not build
from the cost table — but the reason had to be stated correctly, and my first
statement of it was wrong in the direction of dismissing the work.

## Where the rest of the 670 bytes are — measured, one variable at a time

Five declarations over the same 500,000 rows, each adding exactly one thing:

| shape | RSS/row | difference |
|---|---:|---:|
| no index | 1,663 | — |
| `INDEX sku range` | 1,919 | **+256** — one scalar index |
| … `VALUES name` | 2,084 | +165 — the covering copy |
| `ORDERPATH dept→ts` | 2,075 | **+412** — one composite index |
| both indexes | 2,333 | **+670** |

**The decomposition closes**: 256 + 412 = 668 against a measured 670. Two
indexes cost what they cost separately; there is no sharing to reclaim
between them, and a two-byte residual on a 670-byte term says the split is
clean rather than fitted.

**A composite index costs 1.6× a scalar one** — 412 against 256. Nobody had
measured that. It derives an `IndexValue::Str` per row, which is another
`Vec<u8>`, and `tree` and `back` each hold a copy of it just as they do the
key.

Inside the 256 bytes a scalar index spends:

| term | bytes | share |
|---|---:|---:|
| the key, stored twice as `Vec<u8>` (short key) | 68 | 27% |
| `ENTRY_OVERHEAD` as the formula declares it | 48 | 19% |
| **unnamed** | **140** | **55%** |

More than half of a scalar index's cost is somewhere the memory formula does
not account for: `BTreeSet` and `HashMap` node overhead, the third container
(`value_counts`), and allocator rounding on every one of those separate
`Vec<u8>` allocations.

## What this makes worth attacking, in order

1. **The composite index at 412 B/row.** The largest single term, 1.6× a
   scalar index, and unmeasured until now.
2. **The 140 unnamed bytes per scalar index.** `tree` holds
   `(IndexValue, Vec<u8>)` and `back` holds `Vec<u8> → IndexValue` — both the
   value *and* the key exist twice, in two containers that never share. An
   index needs value→keys and key→value; it does not need two independent
   copies of each to provide them.
3. **A2, at ~52 B/row of that 68.** Real, and smaller than both of the above.

None of these should be built from this table either. Each is a Phase A
target that now has a measured size, which is what the gate asked for and
what the 24.6% never had.

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
