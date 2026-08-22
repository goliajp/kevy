# FINDING 2026-08-23 — a hash row's fixed cost does not depend on how many columns it has

**Status**: OPEN as evidence for v5.4 Axis A. Measured, reconciled with the
source decomposition, and the measuring instrument itself corrected mid-way.

## The measurement

Process RSS delta per row, 50,000 rows, one server per configuration
(`target/release/kevy`, no AOF), values ~8 B each plus a 400 B `pad`:

| columns | RSS per row | payload | ratio |
|---:|---:|---:|---:|
| 3 | 1,699 B | 423 B | 4.0× |
| 7 | 1,694 B | 463 B | 3.7× |
| 12 | 1,713 B | 514 B | 3.3× |

**Three columns and twelve columns cost the same 1,700 bytes**, within 14 B,
while their payloads differ by 91 B. The fixed overhead — about 1,230 B once
the payload is subtracted — is flat in the column count.

## Why it is flat

`KevyMap::with_capacity` rounds to `MIN_CAP = 16`
(`crates/kevy-map/src/map.rs:40,189`) and a hash promoted out of the inline
form asks for `with_capacity(1)`
(`crates/kevy-store/src/small_hash.rs:237`), so **every hash from 1 to 14
fields allocates the same 16-slot table**:

| columns | slots | table bytes | live field bytes | wasted |
|---:|---:|---:|---:|---:|
| 3 | 16 | 800 | 144 | 82% |
| 7 | 16 | 800 | 336 | 58% |
| 12 | 16 | 800 | 576 | 28% |

`MIN_CAP` is documented as "≥ 16 (one SSE2 group) so the future SIMD path
can run a full group scan unconditionally" — a cost paid on every small hash
today for a path that does not exist yet. That reasoning is sound for the
**keyspace** table, which is large; it is what makes a per-row value table
expensive.

This is the term the source decomposition attributed 816 B to, now confirmed
from the outside and shown to be **general rather than particular to the
seven-column benchmark row**. Real table rows are three to twelve columns,
which is precisely the range where the waste is 82% down to 28%.

## The instrument had to be corrected first

The same measurement taken through `INFO memory used_memory` produced:

| columns | used_memory per row |
|---:|---:|
| 3 | 1,035 B |
| 7 | 194 B |
| 12 | 226 B |
| 7, 16 B pad | **−464 B** |

A negative per-row cost, and seven columns costing a fifth of three. The
numbers are not noisy — they are *wrong*, and the source decomposition
already said why: `HASH_SLOT_BYTES = 32` against a real 49 B slot, with the
`ArcInner` and the map struct uncharged, under-reporting a seven-field hash
by roughly 344 B (`crates/kevy-store/src/value.rs:442`).

Recording it because the shape of the failure is the recurring one this
session: **the wrong instrument produced output indistinguishable from
data**, and what exposed it was an impossible value rather than a suspicion.
Any budget or tiering decision keyed off `used_memory` inherits this.

## What it means for Axis A

A packed row replaces a fixed ~800 B table plus an `Arc` header with one
allocation of `payload + 2 bytes per column`. The overhead stops being a
constant and starts scaling with the row's actual shape — which is the
structural statement, and the ratios above are its consequence.

Recomputed against the decomposition's terms: 2,282 B → ~905 B per row,
5,821 → ~2,300 KB per MB of source CSV. Split into two steps that can each
be measured and reverted alone:

| step | saving | KB/CSV-MB |
|---|---:|---:|
| A1 — packed row, no per-row table or `Arc` | 36.5% | 5,821 → 3,699 |
| A2 — index entries by dense row id, not key bytes | 24.6% | 3,699 → 2,265 |

Neither has an amortising mechanism in front of it, which is what separated
them from Axis B — whose target turned out to be already amortised by a
batched read, and which measured as noise.
