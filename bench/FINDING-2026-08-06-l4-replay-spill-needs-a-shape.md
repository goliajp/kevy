# B4's replay floor is a number without a shape

Writing the last missing `tiergate` measurement body turned up a
criterion that cannot be answered as written.

## The line

```
L4 replay-spill (B4)  replay-with-spill >= 0.70 x plain replay
```

Every other line in `tiergate` names the shape it asserts over — L6 says
*"5M × 4KiB = 20 GB on a 2 GB budget"*, L2 names its value size, L14
names the concurrent load. **B4 names a ratio and nothing else**, and
the ratio turns out to be almost entirely a function of the shape it
does not name.

## The measurement

One AOF, written once untiered, copied, and replayed twice — once with
tiering off and once on — so both sides replay the identical byte
stream. (Writing the dataset twice would compare two different streams
and call the difference tiering.) Rate comes from the server's own
replay line. ~126 500 keys × 4 KiB ≈ 500 MB, lx64, release build:

| tier budget | dataset : budget | replay rate vs plain | verdict at 0.70 |
|---:|---:|---:|---|
| 64 MB | ~8× | **0.53×** | fails |
| 128 MB | ~4× | **0.61×** | fails |
| 256 MB | ~2× | **0.74×** | passes |
| 512 MB | ~1× (nothing spills) | **1.04×** | passes |

Stable, not noise: three repeats at 64 MB gave 0.55 / 0.57 / 0.55
(plain 250–260 ms, tiered 458–463 ms).

The curve is the explanation. A tiered replay does everything a plain
replay does **and writes the overflow to the vlog on the way**. At 8×
over budget that is ~440 MB of extra writes; at 1× it is none. 1.8×
slower while additionally persisting 440 MB is not obviously a
regression — it may be a good number. But 0.70 cannot be met at 8× and
is comfortably met at 2×, so the line as written asks a question with
no answer.

## What I did and did not decide

**Did:** the body exists (`l4_replay_spill`, run with
`TIERGATE_RUN_L4=1 KEVY_BIN=…`), takes `TIERGATE_L4_BUDGET` so the
shape is explicit at the call site, and defaults to the **harsh** end
(64 MB, ~8× over) so the line reads red rather than flattering itself.

**Did not:** pick the shape B4 should assert. That is the RFC's
sentence to finish, and choosing it silently would make a gate agree
with me instead of with the design. The options are visible in the
table:

* **`≥ 0.70 at 2× over budget`** — what today's engine does, with
  headroom. Asserts that mild overcommit is cheap.
* **a lower floor at a harsher shape** — e.g. `≥ 0.50 at 8×`, which
  today also passes. Asserts that heavy overcommit stays bounded.
* **both, as two lines.** The two say different things and an operator
  cares about both: "does a slightly-over-budget restart feel normal"
  and "does a badly-over-budget restart still finish".

My recommendation is the third, because the single number is what
caused the ambiguity in the first place.

## The pattern worth noting

This is the fourth time this round a claim turned out to be
unfalsifiable-as-stated rather than false: `OFFSET` refused in one doc
and shipped in another, `AUTODECLARE` gated in behaviour but absent
from prose, `RSS ≤ budget × 1.05` conflating a logical bound with a
physical one, and now a perf floor with no workload attached.

None were lies. Each was a sentence written before the measurement
existed, and left alone once it did. **The failure mode is not
dishonesty, it is a claim that never got a second reading after the
thing it described became measurable.**
