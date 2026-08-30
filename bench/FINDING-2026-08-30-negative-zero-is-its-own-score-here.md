# `-0` and `0` are one score in Redis and two here

Found while establishing the correctness precondition for the same-score
ZADD guard (`bench/PERF-DECOMP-2026-08-30-zadd-arena-cell.md`, S04). The
guard was written correctly around it; this is the divergence underneath,
which the guard did not cause and does not fix.

## Reproduced

kevy at develop and `redis:8` in docker, on the same host, one command
sequence each:

| | kevy | Redis 8 |
|---|---|---|
| `ZADD z 0 a` then `ZADD z -0 b`, `ZRANGE z 0 -1` | **`b a`** | **`a b`** |
| `ZADD z2 -0 m` then `ZADD z2 XX CH 0 m` | `0` | `0` |
| `ZSCORE z2 m` after that | `0` | `0` |

The score readings agree. The **order** does not.

## Why

Redis keys its skiplist on a plain `double` comparison, where `-0.0` and
`0.0` compare equal; two members at `±0` are one score and tie-break on the
member string, so `a` sorts before `b`.

kevy's rank tree is a B-tree, and a B-tree key needs a *total* order —
`f64: PartialOrd` is not one. So `Score` implements `Ord` as `total_cmp`,
which is total precisely because it separates `-0.0` from `0.0` and orders
the negative first. That is why `b` sorts before `a` here.

Neither side is careless. Redis can use a partial comparison because its
skiplist never needs a total order over the score alone; kevy's container
does, and `total_cmp` is the correct way to get one. The divergence is what
falls out of the two data structures.

## What it is not

It is not the `CH` short-circuit in `zadd_flags`:

```rust
if *score != old {
    self.zadd(key, &[(*score, m)])?;
    rep.changed += 1;
```

That `!=` is `f64`'s, so a `-0 -> 0` update through `XX`/`CH` is skipped —
and Redis skips it too, for the same reason and with the same reply. The
two agree here. Only the ordering differs.

## The fix, and its one cost

Fold the sign of zero away **where a score enters the sorted set** — the
three encodings' write entry points, `SmallZSetData::try_set`,
`ZSetData::insert` and `SegZSetData::insert`:

```rust
let score = if score == 0.0 { 0.0 } else { score };
```

Once per write. The alternative — normalising inside `Score::cmp` — is a
branch in the innermost comparison of every tree descent, paid on every
read as well as every write, to fix something that can be settled once at
the door.

After it, `-0.0` is unreachable in the store, kevy's order matches Redis's,
and `Score`'s `Eq` and `Ord` agree for every value that can arrive — which
is the invariant `tests_score_order.rs` currently holds by making `Eq` use
`total_cmp`. Both belong: the type-level invariant is what stops the next
caller writing `old == score`, and the fold is what stops the divergence.

## Fixed, and verified the same way it was found

`fold_zero_sign`, called from the top of `zadd_one`. Built on the bench box
and put back in front of the same `redis:8`:

| | kevy | Redis 8 |
|---|---|---|
| `ZRANGE z 0 -1 WITHSCORES` | `a\|0\|b\|0\|` | `a\|0\|b\|0\|` |
| `ZSCORE z2 m` after `ZADD z2 -0 m` | `0` | `0` |
| `ZADD z2 XX CH 0 m` | `0` | `0` |

Byte-identical, where the same three commands gave `b a` here and `a b`
there an hour earlier.

It is its own change rather than part of the perf branch that found it,
because it moves observable ordering and that does not belong inside a
commit about a guard.

## The corpus does not cover it

`tools/check_compat_claim.py` derives the headline from `bench/compat3.sh`,
and that corpus has no `±0` score case — so the claim is honest about what
it measures and this is a hole in what it measures, not a false number. Six lines
now cover it — a `ZADD` with each sign of zero, the `ZRANGE` over the
result with and without scores, the `ZSCORE`, and the `XX CH` update that
both engines decline.
