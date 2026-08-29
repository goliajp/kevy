# FINDING 2026-08-28 — a backreference to a group that never participated matches the empty string

**Status**: OPEN, and open in two projects. Needs a PostgreSQL oracle to
settle; pinned as observed behaviour in the meantime.

## What was observed

`kevy-scalar`'s regex engine is a byte-identical fork of spg's ERE matcher.
Writing capture-group tests for it turned up a case neither corpus covers:

| pattern | input | engine |
|---|---|---|
| `^(?:(a)\|b)\1$` | `b` | **matches** (0,1) |
| `^(?:(a)\|b)\1$` | `aa` | matches (0,2) |
| `^(?:(a)\|b)\1$` | `ba` | no match |
| `(a)?b\1` | `b` | **matches** (0,1) |

So a backreference whose group did not participate behaves as an empty
match rather than as a failure. The third row matters: it is still a real
backreference — `ba` is refused — so this is not the reference degrading
into a wildcard.

## Why it is not simply a bug to fix here

The engine arrived byte-identical from spg
(`spg/crates/spg-engine/src/eval/regexp.rs`), and the fork's value is that
it stays that way: a change made here and not there splits two matchers
that are supposed to be one. **If this is wrong, it is wrong in spg too**,
and that is where it should be fixed and forked forward from.

## What is not established

Whether PostgreSQL agrees. PG's ARE comes from Henry Spencer's regex, and
its treatment of a backreference to an unset group is exactly the kind of
corner where implementations diverge. Neither kevy's funcgate corpus nor
spg's e2e corpus has a case for it, which is why it went unnoticed on both
sides: 438 never-executed regions in `caps.rs` is a lot of room for a
question nobody was asking.

I did not guess. The test that found this began as an assertion — written
from what I believed POSIX required — and the engine refused it. Asserting
my belief over the engine would have been inventing an oracle; asserting the
engine's behaviour as correct would have been the same thing pointed the
other way. It is pinned as **observed**, labelled as such in the test, with
this note attached.

## How to settle it

One `psql` line against PG 18.x:

```sql
SELECT 'b' ~ '^(?:(a)|b)\1$', 'b' ~ '(a)?b\1';
```

If PG says false, the fork diverges from PG and spg owns the fix. If PG says
true, the pinned test becomes a sourced one and this finding closes.

## Where it came from

The capture matcher was the largest unexercised block in the crate — 438
never-executed regions, and the least defensible ones, since kevy reaches
`re_find_caps` on every `regexp_matches` and every `regexp_replace` with a
`\1`. That is live path, not spare capacity carried for fork fidelity.
