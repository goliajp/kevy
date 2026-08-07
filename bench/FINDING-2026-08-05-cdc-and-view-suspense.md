# CDC and VIEW: two surfaces the docs advocate and no consumer uses

`bench/R4A-SQL-SHAPE-INVENTORY-2026-07-31.md` closed with two items in
its suspense column, both of the same shape — *"advocacy exists, field
evidence does not"*:

* **CDC / `FEED.*`** — three cookbook recipes and a strong claim ("the
  outbox you don't need"), but neither embedding consumer calls
  `FEED.READ` even once.
* **`VIEW.*`** — views.md calls the hot list its killer app; mailrs
  covered all fourteen of its list axes with `ORDERPATH` + flag indexes
  and declared **zero** views.

The v5 ledger asked for both to be re-asked on the RDS axis. They have
different answers.

## CDC: the surface is fine, the evidence was pointed at the wrong thing

"No consumer" reads like "unproven". It is not the same claim, and the
distinction matters:

* **The recovery contract is gated.** `docs/cdc.md`'s central promise —
  *snapshot S + the feed frames from S's cursor = the exact state* — is
  executed by `bench/restore-drill.sh` (boot, phase-1 writes, `SAVE`,
  phase-2 writes, capture frames from each shard's snapshot cursor,
  kill, restore into a fresh dir, replay, then a **full per-key value
  compare**), wired into `bench/diskgate.sh`. That is a real drill, not
  an assertion.
* **The stream's completeness was not.** Which is where a hole was:
  until today, a **cross-shard `RENAME` emitted nothing at all** — the
  destination write carried no AOF record and no feed frame. A consumer
  rebuilding state from frames would keep the old key forever and never
  learn the new one. Verified against `develop`: the destination shard
  returns an empty frame list. Now held by
  `feed_cdc::cross_shard_rename_reaches_the_feed_on_both_ends`.

So the re-asked answer is: **CDC's contract is verified; its coverage
was not, and coverage is the half that silence hides.** A consumer would
have caught this on day one. Nobody had one, so a test has to stand in —
and the missing verb was found by the same "write on one path, read on
another" lens that produced the five correctness bugs on this branch,
not by the feed's own test file, which was green throughout.

**Worth noting for the ledger:** this is the fourth reader of the write
path found to disagree with it today (AOF replay, replica apply,
snapshot round-trip, change feed). The pattern is not "the feed is
undertested" — it is that **every derived reader needs the same
round-trip gate**, and only AOF had one.

## VIEW: the advocacy is aimed at a shape the cheaper primitive covers

views.md motivates views with:

> `WHERE state = 'ready' AND pri BETWEEN 0 AND 100 ORDER BY pri DESC
> LIMIT 10`

That is an equality prefix plus one range on the next column — exactly
what a composite `ORDERPATH` represents as a single contiguous walk.
Measured, not argued (scratch instance, four rows, three `ready`):

```
TABLE.DECLARE job PREFIX job: PK id COLUMN id str COLUMN state str
    COLUMN pri i64 ORDERPATH ready_by_pri ON state THEN pri DESC
IDX.QUERY job.ready_by_pri WHERE state EQ ready RANGE pri 0 100 LIMIT 10
→ job:3, job:2, job:1   (pri 90, 60, 30 — DESC; the blocked row absent)
```

One B-tree, no maintained second structure, no rebuild to verify.

**So mailrs declaring zero views is not an adoption gap — it is the
correct call, made without the docs' help.** `views.md` never mentions
`ORDERPATH` at all, so a reader arriving with a hot list is pointed at
the heavier of the two primitives by a page that does not know the
lighter one exists.

What a view is genuinely *for* — none of which an ordered composite key
can be:

| shape | why the composite cannot |
|---|---|
| `OR` across indexes | one leading column; a union is not a walk |
| `DIFF` | set subtraction has no ordered-key form |
| two ranges on different columns | a composite ranges on one column, at the tail, and nothing may follow |
| bounded always-warm answer | `MODE materialized TOPK n` fixes the read cost regardless of selectivity |

Fixed here: `docs/views.md` opens with **"First: check whether you need
one"** — the ORDERPATH alternative, the runnable example above, the
four shapes that do need a view, and the plain statement that a view is
the more expensive of the two by construction.

## What this says about the RDS thesis

Both items resolve the same way, and it is not the way the suspense
column implied. Neither surface is speculative:

* CDC's contract holds and now has the coverage test it lacked.
* VIEW answers four shapes nothing else answers.

**The defect in both cases was the documentation pointing users at the
wrong thing** — advocating a surface without stating when *not* to
reach for it. For an engine whose differentiation is *you declare your
access paths, so you do not need a DBA*, "which primitive do I declare?"
is not a footnote. It is the whole job the DBA used to do.

That makes this the third instance today of the same failure mode: a
correct engine described by a page that drifted from it
(`FINDING-…-offset-boundary-drift`, `FINDING-…-autodeclare-boundary`,
this). Two of the three were mechanically detectable and are now gated
(`tools/check_doc_links.py`). This one was not — no gate can tell you a
page recommends the wrong primitive.

**Left open (owner's):** whether `VIEW.*` in its virtual mode still
earns its surface once the "use an ORDERPATH" guidance is in place, or
whether the maintained TOPK mode is the whole value. That is a
scope call, and removing surface is never mine.
