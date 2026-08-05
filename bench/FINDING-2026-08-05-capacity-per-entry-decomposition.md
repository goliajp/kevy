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
size-class rounding. So:

> **stub ≈ 90 B fixed + the key.**

The window model in the master plan estimates **~1 B/entry** of resident
cold cost (≈200 B per segment + one `(28 B + key)` fence per 4 KiB
page). That figure is about the cold **index** — and it is not wrong,
it is about a different structure. What actually bounds capacity is the
**tier stub**: the in-memory record that says where a demoted value
lives. It is ~90× larger per entry than the number the capacity model
carries, and the model does not mention it.

## The formula, and it predicts the measured ceiling

If the floor is `entries × (90 B + key)` and the ceiling is the budget,
then:

> **max data:RAM ≈ value_size / (90 B + key length)**

Checked against the lx64 sweep, which knew nothing about this model:

| | predicted | measured |
|---|---|---|
| ceiling ratio, 4 KiB values / 9 B keys | **42.7×** | **39.2×** saturation |
| entries a 2 GB budget holds | **23.5 M** | crossing at **21.6 M** |

Within 9 %, and the residual is exactly what the model leaves out (the
hot values and index still resident alongside the stubs).

## What this means for the capacity claim

The claim now has a shape rather than a single number, and the shape is
uncomfortable in a useful way:

| value size | key | ceiling |
|---:|---:|---:|
| 256 B | 9 B | **2.7×** |
| 4 KiB | 9 B | 42.7× |
| 4 KiB | 48 B | 30.3× |
| 64 KiB | 9 B | 682.7× |

**A tiering ratio is a statement about value size, and at small values
there is barely a ratio at all.** "This 8 GB machine holds 200 GB" is
true for 4 KiB values and false — by more than an order of magnitude —
for 256 B ones. Any external statement has to carry the value size, or
it will be wrong for exactly the workloads (small records) an SME is
most likely to have.

## Left open — and now it is a design question, not a measurement one

The stub is ~90 B of fixed overhead for what is conceptually a file id,
an offset and a length. That is the number to attack if the ratio at
small values matters, and it is a **`kevy-alloc` / layout** question,
not a tiering-policy one — which is the third independent line this
round that arrives at the allocator (the others being RSS fragmentation
at 1.34→2.22× and the v5 vision's own first priority).

Whether small-value tiering is a workload kevy wants to claim at all is
a scope call, not mine. But it should be made knowing that today the
answer is 2.7×, not 39×.
