# FINDING 2026-08-23 — merging the shard pages instead of sorting them: +93% on the list page

**Status**: CLOSED, positive. Two attacks, isolated and measured
separately, both wins, and they compose. The list page at 32 and 64 clients
changes hands.

## What was attacked

Two of the four non-structural terms the list-page decomposition named
(`.claude/rfcs/2026-08-22-rds-side-representation-and-paths.md` §4):

1. **The origin sorted what was already sorted.** Every shard walks its own
   tree in `(value, key)` order and stops at LIMIT, so each chunk arrives
   sorted. The reduce flattened all sixteen into one `Vec`, sorted the lot,
   and truncated to 20 — 320 rows decoded and sorted to keep 20, with a heap
   allocation per key, per value and per hydrated field on each of the 300
   that lose. The function's own doc comment has always said "k-way merge".
   It is one now: LIMIT + N decodes instead of N × LIMIT.
2. **The fan-out cloned its argv per shard.** `Op::Extension` carried
   `Vec<Vec<u8>>`, so dispatching one query to sixteen shards copied the
   same ten byte-strings sixteen times — about 160 allocations for bytes
   nobody mutates. It is an `Arc<[Vec<u8>]>` now.

## Result — three arms, five interleaved passes, c=64, tiered

| | page ops/s | page p50 | page p99 |
|---|---:|---:|---:|
| develop | 35,385 (±9.6%) | 1,559 µs (±8.5%) | 2,792 µs (±15.0%) |
| merge only | 53,912 (±2.4%) | 933 µs (±1.0%) | 2,081 µs (±6.3%) |
| merge + shared argv | **68,182** (±3.2%) | **728 µs** (±3.2%) | **1,656 µs** (±13.3%) |

**+93% throughput, −53% p50, −41% p99** — and every arm's variance is
*tighter* than develop's, which is the opposite of what a latency/throughput
trade would look like.

The two compose rather than overlap: the merge alone is +52%, the shared
argv adds another +26% on top of it.

Serial axis, median of two passes each with the order reversed
(`bench/pgcompare.sh`, 2M × 440 B):

| | develop | after | |
|---|---:|---:|---:|
| idx p99, `everysec` | 350 µs | 159 µs | **−54.5%** |
| idx p99, `tiered` | 350 µs | 192 µs | −45.1% |
| page p50, `none` | 179 µs | 104 µs | −41.9% |

PostgreSQL's rows in the same runs did not move (page p99 147/155 against
158/145), which is the witness that the box was not drifting under the
comparison.

### It changes the standing on the one shape PostgreSQL owned

| clients | PostgreSQL | develop | after |
|---:|---:|---:|---|
| 32 | 54,080 | 35,788 → PG wins 1.51× | 56,281 → **kevy 1.04×** |
| 64 | 47,198 | 36,680 → PG wins 1.29× | 64,289 → **kevy 1.36×** |

The list page was the last read shape PostgreSQL won at concurrency. It no
longer is.

## The hypothesis this refuted along the way

The first pair of passes suggested the change had made the tail *bimodal* —
page p99 landing either far below develop or far above it, run to run,
where develop was tight. The suspected mechanism was the shared `Arc`: a
refcount that sixteen shard threads now touch on one cache line, where
before each held its own `Vec`.

A third binary was built with the merge but without the shared argv
(`51030a4a` on `feature/idx-page-merge-only`, kept rather than deleted so
the control arm stays reproducible), and the three arms were run five times
interleaved. **The hypothesis is refuted.**
The shared-argv arm is the *best* of the three on every statistic including
p99, and its spread is no worse than develop's.

What the isolation did show is that the intermittent spike is **not this
work's**: one run in five spikes on `idx p99` in *every* arm, develop
included — `[1093, 993, 1160, 1205, 4839]` on develop against
`[814, 731, 672, 656, 3948]` on the merge arm. That is a pre-existing
intermittency of the box or the engine, and attributing it to a change under
test was an error of attribution that only five interleaved passes could
correct. It is logged as an open item, not as a cost of this change.

## Gates

Five RDS gates green against the attacked binary on lx64:
`idxgate` (IDX.QUERY p99 median 0.35 ms, **worst-conn 1.85 ms against
3.36 ms before**; bytes/row ratio 1.01), `viewgate` (virtual 0.50 ms,
materialized 0.33 ms, write tax 2.0%), `agggate` (GROUP p99 0.130 ms,
GROUPS top-100 2.025 ms, write tax 10.2% AT-THE-LINE as before),
`servinggate` (row-list 0.266 ms, view 0.156 ms, write fan-out 85 µs),
`tablegate` L1/L2/L3/L6/L7.

## A note on how nearly this was measured wrong

The first A/B compared a binary with itself. The checkout that was meant to
switch arms was blocked by files copied into the tree by hand, and its
output was piped into `tail`, which swallowed the exit code — so two builds
of the same commit reported as two arms, at the same second, and the numbers
that came out were a perfectly ordinary-looking null result.

The rerun computes both binaries' digests and refuses when they match. It is
the same lesson the allocator A/B taught and the fsync probe taught twice:
**a measurement device that has failed produces output shaped exactly like
data**, and the only defence is a check that fails loudly for a reason
unrelated to what is being measured.

## What is still on the list

Two of the four terms remain, both from the same decomposition:

- **Hydration runs per shard, before truncation** — up to 320 row reads of
  which ~300 are discarded. Moving it after the merge costs 20. Needs a
  protocol change (the shard would send unhydrated rows, or the origin would
  ask the winners' shards in a second phase), so it is a design item rather
  than a local edit.
- **`FIELDS` ignores the covering values the segment already holds** and
  probes the keyspace instead. An index-only scan is on the table and not
  taken — and by the memory decomposition those covering bytes are already
  paid for.
