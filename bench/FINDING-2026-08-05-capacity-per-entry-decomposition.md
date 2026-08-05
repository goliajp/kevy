# The per-entry cost never accelerated — the budget was hiding it

`FINDING-2026-08-05-capacity-ceiling-sweep.md` closed with one thing it
could not explain:

> 18.7 B/entry then 104 B/entry is a fivefold step, and the sweep
> measures it without accounting for it. That is the next question, and
> it is a decomposition question, not another rung.

It is answered, and the answer removes the step: **there was no
acceleration.** The cost was ~104 B/entry the whole time. Below
saturation the budget was masking it.

## The measurement

The bend is about entry count, not absolute size, so it reproduces on a
laptop: one shard-pair, a **16 MB** tier budget, 4 KiB values, ~9-byte
keys, reading `INFO Tiering` at each step.

| entries | `used_memory` | `stub_bytes` | marginal B/entry |
|---:|---:|---:|---:|
| 100 000 | 10.5 MB | 9.6 MB | — |
| 200 000 | 20.7 MB | 19.2 MB | 102.4 |
| 400 000 | 41.4 MB | 38.3 MB | 103.3 |
| 600 000 | 62.5 MB | 57.5 MB | 105.7 |
| 800 000 | 83.2 MB | 76.6 MB | 103.6 |
| 1 000 000 | 104.2 MB | 95.8 MB | 105.1 |
| 1 200 000 | 124.9 MB | 115.0 MB | 103.1 |

**Flat at ~104 B/entry from the first step**, and `stub_bytes` is
essentially all of it: 115.0 MB / 1.198 M = **96.0 B of stub per
entry**. The lx64 sweep measured 104.3 B/entry over its upper segment.
Two machines, two scales, 100× apart in dataset size, same constant.

## Why the lower segment read 18.7

`used_memory` counts stubs *and* whatever values are still hot. Below
saturation, each new entry does two things at once:

* adds a stub — **+96 B, permanent, cannot be demoted**;
* pushes the store over its budget, so the tier demotes a value —
  **−4 KiB, freed**.

The two nearly cancel, and `used_memory` sits pinned at the budget. The
18.7 B/entry "marginal cost" was never a cost — it was the *residual* of
a growing floor against a shrinking ceiling.

Once every value is already cold there is nothing left to demote, the
subtraction stops, and the floor's real slope appears undamped. Nothing
sped up; the masking ran out.

**That reframes the ceiling.** It is not "the engine degrades past 39×".
It is: **the non-demotable floor rises at a fixed rate, and the ceiling
is where it meets the budget.**

## What a stub costs, and what the capacity model was missing

Same setup, 48-byte keys instead of 9:

| key length | stub per entry |
|---:|---:|
| 9 B | 96.0 B |
| 48 B | 142.7 B |

39 extra key bytes cost 46.7 B — the key itself plus allocator
size-class rounding.

**And this constant is not a discovery — it is already gated.**
`bench/memgate.sh`'s B7 line states the formula outright:

> stub bytes/entry ≈ `ENTRY_OVERHEAD(96)` + key heap bytes, ±band

with the note that keys under the 22-byte inline boundary carry zero
key heap, so the formula is *exactly 96*. The 9-byte-key measurement
above reproduces that to the decimal; the 48-byte measurement adds the
key heap the formula predicts.

### And the name on it is wrong, which changes where the lever is

`ENTRY_OVERHEAD` is **not the stub's cost**. Reading its definition:

> Per-entry overhead in the top-level keyspace map: the inline 24-byte
> `SmallBytes` key cell + the 64-byte `Entry` + metadata.

The stub itself is free of heap and 24 bytes **inline in that same
`Entry`** (`Value::Cold(_) => 0`, with the comment *"a cold key weighs
key-heap + ENTRY_OVERHEAD only"*). So the 96 B is what **every key in
kevy costs whether it is tiered or not**; tiering reclaims the value
and cannot reclaim the entry that names it.

That reframes the ceiling and, more usefully, moves the lever:

* It is not "the stub is expensive". It is **"tiering can return the
  value, never the key"** — an unavoidable consequence of the keyspace
  being in memory, not a tiering inefficiency.
* So the number to attack, if the small-value ratio matters, is the
  **keyspace entry** (24 B key cell + 64 B `Entry`), which is a
  store-wide change touching every key in every workload — not a knob
  inside the tier. Anything that shrinks it improves untiered memory
  too, and anything that breaks it breaks everything.

**What was missing is what the number implies.** The floor was
documented as a floor — a quantity to subtract from a budget when
sizing — and never turned into the ceiling it dictates. Everything
below is that step, and it is why the sweep's own numbers looked like
an acceleration to whoever read them (me) without it.

The window model's separate **~1 B/entry** figure (≈200 B per segment
plus one `(28 B + key)` fence per 4 KiB page) is about the cold
**index**, a different structure, and is not in tension with this.

## The formula, and it predicts the measured ceiling

If the floor is `entries × (96 B + key heap)` and the ceiling is the
budget, then:

> **max data:RAM ≈ value_size / (96 B + key heap)**
>
> — the same 96 B `memgate` already gates: the cost of the **keyspace
> entry**, which tiering cannot reclaim, with key heap 0 for keys under
> the 22-byte inline boundary.

Checked against the lx64 sweep, which knew nothing about this model:

| | predicted | measured |
|---|---|---|
| ceiling ratio, 4 KiB values / 9 B keys | **42.7×** | **39.2×** saturation |
| entries a 2 GB budget holds | **23.5 M** | crossing at **21.6 M** |

Within 9 %, and the residual is exactly what the model leaves out (the
hot values and index still resident alongside the stubs).

### Then tested where it actually matters, rather than trusting it

A formula derived from one workload and extrapolated is exactly what
this round has already been wrong about twice (a "plateau" that was a
staircase, a ceiling estimate off by 14 %). So the small-value end — the
one that changes the product claim — was measured, not asserted. Same
16 MB budget, same keys, only the value size varied:

| value size | predicted | **measured** |
|---:|---:|---:|
| 256 B | 2.67× | **2.65×** |
| 1 KiB | 10.7× | **10.43×** |
| 4 KiB | 42.7× | **39.2×** (lx64, full scale) |

The model holds across a 16× span of value sizes. The 4 KiB point sits
slightly under prediction because at full scale more than stubs is
resident; the two small-value points land within 3 %.

**And at 256 B the budget is not merely approached, it is abandoned.**
`used_memory` passes the 16 MB budget by 200 000 entries and then climbs
linearly — 77 MB at 800 000, nearly 5× the budget — because demoting a
256 B value frees less than the 96 B stub it leaves behind plus the
accounting it takes to get there. There is no configuration of this
workload where the tier holds its bound.

## What this means for the capacity claim

The claim now has a shape rather than a single number, and the shape is
uncomfortable in a useful way:

| value size | key | ceiling | source |
|---:|---:|---:|---|
| 256 B | 9 B | **2.65×** | measured |
| 1 KiB | 9 B | **10.43×** | measured |
| 4 KiB | 9 B | **39.2×** | measured (lx64) |
| 4 KiB | 48 B | 30.3× | model |
| 64 KiB | 9 B | 682.7× | model |

**A tiering ratio is a statement about value size, and at small values
there is barely a ratio at all.** "This 8 GB machine holds 200 GB" is
true for 4 KiB values and false — by more than an order of magnitude —
for 256 B ones. Any external statement has to carry the value size, or
it will be wrong for exactly the workloads (small records) an SME is
most likely to have.

## Left open — and now it is a design question, not a measurement one

The binding cost is the **keyspace entry**, not the stub: 24 B of key
cell plus a 64 B `Entry`, paid by every key whether tiered or not. That
is the number to attack if the ratio at small values matters, and it is
a **store-layout** question — wider and riskier than a tiering knob,
because it changes what every key costs in every workload, tiered or
not. It is also the third independent line this round arriving at
memory layout (the others: RSS fragmentation 1.34→2.22×, and the v5
vision's own first priority).

**And it bounds what any tiering work could ever buy.** No policy
change inside the tier moves this number, because the tier already
returns everything it can: the value. A 256 B workload is at 2.65×
because 256 B of value sits against 96 B of key that cannot leave.

Whether small-value tiering is a workload kevy wants to claim at all is
a scope call, not mine. But it should be made knowing that today the
answer is 2.7×, not 39×.
