# The 3.x support line, and where its durability boundary is

Written in answer to R5 of
`REPORT-FROM-GOLIAJP-2026-07-20-EMBEDDED-AS-PRIMARY-STORE.md` — "publish
v4.0.0, or state the 3.x support line" — and to give a schedule you can
plan against rather than a release date you have to wait for.

**Short version: the D1 fix can reach 3.x; the D2 fix cannot.** That is
not a scheduling choice, it is a format dependency, and it is the thing
worth knowing before you decide which line to sit on.

## What was fixed, and where it can live

| Fix | On `feature/v4` | Portable to 3.18.x? | Why |
|---|---|---|---|
| **D1** — rejected `atomic()` rolls back | done | **yes** | needs `take_with_ttl` / `put_with_ttl`, both present in 3.18 |
| **R2** — collection reads inside `atomic()` | done | **yes** | `SMEMBERS`, `SISMEMBER`, `LRANGE`, `LLEN`, `SCARD`, `ZRANGEBYSCORE`; the op table has the same shape in 3.18 |
| Group commit wiring | done | **yes** | `Aof::begin_group` / `end_group` already exist in 3.18 |
| **D2** — transactions replay all-or-nothing | done | **no, not as built** | the markers ride as AOF **v2** records, and 3.18 has no v2 envelope |

## The D2 boundary, stated precisely

The fix that actually makes `atomic()` crash-atomic is a pair of
transaction markers in the log: replay holds every frame after a begin
marker and applies the batch only on the matching commit. That makes
"did this transaction finish" a property of the log.

The markers are AOF **v2** records — the `KEVYAOF2` envelope with a
per-record length and CRC32C, which arrived in the 4.0 durability work.
**3.18 writes v1: bare RESP, no envelope.** Grepping the 3.18 tag for the
v2 format returns nothing.

So a 3.18.x patch could carry group commit, and **group commit alone is
not enough** — which I know because I shipped exactly that mistake first
and had to correct it. Measured on this machine, group commit only, a
20,000-mutation block killed mid-commit:

```
kill@12ms -> 6393/20000
```

The AOF writes through a 256 KiB buffer. Group commit defers the *fsync*,
but frames still reach the kernel as that buffer fills, and `kill -9`
leaves them there. So a 3.18.x with group commit would give you:

> `atomic()` is crash-atomic **for transactions whose frames fit in 256
> KiB**, and silently is not beyond that.

A guarantee with an invisible size cliff is worse than a stated absence,
particularly for the range-overlap constraint you described. I am not
willing to ship that shape and call D2 fixed.

Porting the markers to v1 is possible — bare RESP can carry a marker
frame, and the v1 replay path could buffer the same way. It would mean
new, less-exercised code on a format 4.0 has already replaced, on the
durability path, for one release. If you want that, say so and it is a
real option; I would not choose it unprompted.

## What this means for your planning

- **If you stay on 3.18**: a 3.18.x can give you D1 (rollback — the one
  that unblocks the cookbook §5 CHECK pattern and lets you delete your
  read-before-write discipline) and R2 (collection reads — the one that
  lets child collections be sets again). D2 stays open, and your
  boot-time reconciliation from R4 stays load-bearing.
- **If you move to 4.0**: you get D2 as well, and the AOF format change
  is one-way — a 3.x data dir opens and upgrades on first rewrite, but
  not back.
- **Neither is released today.** 4.0's release train (channels, packaging,
  final review) is not finished, and I will not call CI-green a release.

## Honest status of the 4.0 fixes themselves

- D1 rollback: 5 unit tests, plus your exact reproduction, which now
  agrees on both lines.
- D2 markers: 4 unit tests, and crash-verified with a harness of your
  shape at 3× and 15× the write buffer — only 0-or-all across 22 sampled
  kill offsets. **Not power-loss-verified**: `kill -9` leaves the page
  cache intact, so these runs exercise process death, not media loss.
- R2 collection reads: 3 tests, including one that checks the reads do
  not interfere with rollback (they share a context).

## What I would like from you

If the 256 KiB-cliff shape would actually be useful to you as an interim
— i.e. your transactions are small and you would rather have partial
crash-atomicity than none while you finish the migration — that changes
the calculus and a 3.18.x becomes worth building. You know your
transaction sizes; I do not.
