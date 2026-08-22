# FINDING 2026-08-23 — answering FIELDS from the index's copies is not measurable here

**Status**: CLOSED, no effect. The change is correct and its tests are
worth keeping; its performance claim is not supported and must not be made.

## What was expected, and why

`FIELDS` probed the keyspace per key even when the index already stored
every requested column as a covering `VALUES` copy. For a **tiered** row
that probe is a disk read for bytes already resident in the segment, so the
prediction was that tiered would gain the most.

## What was measured

`bench/pgcompare.sh 2000000 400` on lx64, two arms with distinct digests
proven before either ran, three interleaved passes each, against the 5.4
baseline.

| shape | develop | index-only | |
|---|---:|---:|---:|
| idx p50 | 112–115 µs | 111–122 | −0.9% … +6.1% |
| idx p99 | 153–205 | 157–206 | ±3% |
| page p50 | 148–165 | 147–169 | ±2% |
| page p99 | 196–240 | 203–293 | −8.6% … +22% |

**No term improved, and tiered — the arm predicted to gain most — is the
one that looks worst.**

Per-pass, though, the sign does not hold. `everysec` page p99 is
`[195, 196, 303]` on develop against `[229, 226, 198]` on the change: the
ranges overlap and the +15% is where the medians happened to land. The
tightest term in the whole run, `idx p50` at ±1%, has the change slightly
ahead (`[111, 111, 111]` against `[110, 112, 113]`).

PostgreSQL's rows did not move across the arms, so the box was not drifting
under the comparison.

**The honest reading: the effect is inside the noise. Not an improvement,
not a regression.**

## Why the prediction was wrong

Unmeasured, and this finding does not guess between the candidates:

- `peek_hash_rows` coalesces cold rows into **one** batched submission for
  the whole page (`crates/kevy-store/src/tier_serve.rs:346`), so a page of
  20 cold rows is one I/O, not twenty. A per-key `RowValues` hash lookup
  need not beat that.
- The index-only path `to_vec()`s each covering value per hit, where the
  row path may hand out borrows.

Either would explain it; distinguishing them needs a profile, and asserting
one without would be the hand-wave the methodology bans.

## What is kept, and what is not

**Kept — it is a correctness fix, not a performance one.** The work that
found this also found that a covering `VALUES` copy outlived the field it
copies: after `HPEXPIRE`, `HGET` answered nil while `FILTER` went on
selecting the row by that value, because the reaper's list of expired keys
was discarded with `let _ =`. That is fixed and gated
(`a_covering_value_does_not_outlive_the_field_it_copies`), and it stands on
its own.

**Reverted — the index-only read path.** It moves nothing measurable, and
keeping a code path because it *ought* to be faster is how a codebase
accumulates complexity that no measurement asked for. The byte-identity
test and the shared slot encoder go with it.

## What this says about the RFC's sequencing

`.claude/rfcs/2026-08-23-v5.4-use-the-declaration.md` §8 put this first as
"the smallest thing that moves a measured number". It was the smallest; it
did not move one. The reason is worth carrying into the remaining axes:
**the hydration cost this axis attacked is already amortised by a batched
read**, so per-row savings on that path have little left to take. Axis D
(the record format) and Axis A (the packed row) attack terms that are
per-record and per-row with no such amortisation in front of them — a
distinction the sequencing did not make and now should.
