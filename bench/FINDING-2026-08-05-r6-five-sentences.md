# R6: the five operational sentences, checked one at a time after windowing

R6's criterion, from the master plan, is not a feature list:

> **the five sentences — one binary / one config / backup is copying
> files / upgrade is swapping the binary / no tuning — must each still
> be true after windowing, with e2e backing.**

Windowing added a second on-disk truth-adjacent structure (cold index
segments under `segs-<shard>/`, cold values under `tier/<shard>/`), so
each sentence had to be re-earned rather than inherited. Measured, not
argued:

| # | sentence | verdict |
|---|---|---|
| 1 | one binary | **holds** |
| 2 | one config | **holds** — and the windowed runs used none at all |
| 3 | backup is copying files | **holds**, for a reason worth stating |
| 4 | upgrade is swapping the binary | **holds, both directions** |
| 5 | no tuning | **holds with one named exception**, already measured |

## 1. One binary

Every measurement in this round — declaring a windowed table, sliding it,
sealing cold segments, demoting values to the vlog, serving hot+cold
range queries, `TABLE.VERIFY`, backup and restore — ran against the
single `kevy` binary with no sidecar, no compaction daemon, no external
index service. Windowing added structures, not processes.

## 2. One config

The windowed correctness runs (`TABLE.DECLARE … WINDOW ts SPAN 2000
BUCKET 500`, 20 000 rows, sliding throughout) were started with nothing
but `--port`, `--threads` and `--dir`. **No config file existed.** The
window is *declared with the table*, on the data plane, not configured;
the only operational knob the tier introduces is the memory budget
(`KEVY_TIER_BUDGET` / the config's tier section), and it is a budget
statement rather than a tuning parameter — the capacity sweep varied it
as the independent variable precisely because it *is* the declared
bound.

## 3. Backup is copying files

`kevy-cli backup` packs the data directory and **skips subdirectories**,
which is where the whole cold tier lives. It is nevertheless complete,
and the reason is the interesting part: a snapshot **materialises** cold
values rather than referencing them.

Measured end to end (20 000 × 4 KiB against a 64 MB budget, demotion
engaged at `used_memory` 60.4 MB): `SAVE`, back up (5 files, no `tier/`),
restore into a fresh directory, boot → `DBSIZE` 20000, and `STRLEN` of
keys that had been demoted returns 4096 each.

The derived-spill claim is what makes this safe, and it is now checked
rather than assumed. See
`FINDING-2026-08-05-windowed-index-loses-rows.md` for the full
measurement; the one thing to fix is `pack()`'s comment, which still
says the data dir is flat.

## 4. Upgrade is swapping the binary

The sharpest version of this question after windowing: can a binary read
a data directory another version left behind **including its cold
segments**? Tested in both directions with a real windowed table that
had slid (two `.seg` files per shard on disk):

| direction | result |
|---|---|
| pre-fix binary writes 6 000 rows → **new** binary boots the same dir | `DBSIZE` 6000, index reachable **6000/6000**, `TABLE.VERIFY` clean (`missing` 0, `drift` 0), table catalog intact, spot rows correct |
| new binary appends to 7 000 → **pre-fix** binary boots the same dir | `DBSIZE` 7000, index reachable **7000/7000**, spot rows correct |

So the sentence holds forward *and* backward across this round's
changes. That is not luck: the tombstone fix changed an in-memory
representation, and cold segments are derived spill that a restart
re-earns — `clean_stale_derived` drops a previous run's segments at
boot, so no segment format can strand an upgrade.

(The writes were paced so buckets sealed quietly, which keeps the
pre-fix binary's own tombstone defect out of the comparison. Testing an
upgrade against a binary that loses rows on its own would have measured
the wrong thing.)

## 5. No tuning — with one exception, named

Everything above ran on defaults. The windowed correctness runs, the
backup drill and the upgrade drill used no tuning at all; the capacity
sweep set only the budget it was measuring.

**The exception is already on the record and should stay named rather
than quietly dropped:** R3 measured an idx p99 cliff (371 µs) that came
from *oversubscription*, and `--threads ≤ cores − 2` brought it back to
98 µs. That is a deployment shape, not a per-workload knob — you set it
once from the machine's core count — but it is a number an operator has
to know, so "no tuning" is honest only when stated as **"no per-workload
tuning; one deployment-shape rule."**

## What this closes

R6's criterion asked for five sentences with e2e backing. Four are now
backed by measurements taken this round, and the fifth is backed with an
exception measured earlier and carried forward rather than dropped.

**Left for R6:** the deployment recipe (C3, caddy in front) and the
verifiability narrative are documentation work, not measurement — and
the verifiability narrative should now cite this round's evidence that
the audit itself had to be made evidence-based before it could be
trusted.
