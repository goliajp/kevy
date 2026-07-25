# Capacity arc T9 full-scale envelope — synchronous vlog compaction stalls the reactor

**Status:** ROOT CAUSE IDENTIFIED (source-decisive), perf-record confirmation
in flight. Fix in progress: budget the compaction the same way spill is
budgeted. Release of the capacity headline is PAUSED until the envelope
passes at full scale.

## Measured failure (lx64, `CAPACITY_SCALE=full`, 10M rows / ~1KiB / 3GB budget)

The T9 envelope's correctness columns all pass (D2 index-only = 0 cold reads,
hydration one-pread-per-row, promotions=0, RSS bounded). The **latency SLAs
fail by 50–500×**, and B6's 20GB-into-2GB load cannot finish in 600s:

| SLA | target | measured p99/p95 | p50 |
|---|---|---|---|
| C4 point lookup p99 | ≤ 1 ms | **68.3 ms** | 132 µs |
| C5 FILTER+SORT page p95 | ≤ 5 ms | **69.7 ms** | 634 µs |
| D1 hydration page p95 | ≤ 10 ms | **63.0 ms** | 904 µs |
| B2 cold hash-row p99 | ≤ 500 µs | **52.9 ms** | 116 µs |
| B6 20GB load on 2GB budget | completes | **driver timeout > 600 s** | — |

The distribution is **bimodal**: p50 is healthy everywhere (~90–900 µs), the
tail is a uniform ~35–82 ms wall that hits **hot reads too** (hot baseline
p99 = 55.8 ms). Healthy p50 + uniform fat tail across unrelated query shapes =
an occasional multi-ms stall on the shared shard reactor, not a per-op cost.
The earlier demotion-sampler bound (write-path visit window = 512) removed one
O(map) scan but did **not** move these tails — a second, larger stall remained.

## CORRECTION (perf-confirmed) — the stall is a per-tick O(rows) rescan, NOT compaction

The compaction theory below was **refuted by measurement**: budgeting
compaction (bounded per-tick steps) left the tails unchanged, and the
faithful repro (load + declare + backfill + a read-only c4 sweep) creates
no dead vlog records, so compaction never even runs during it. Two
source-only root-cause guesses (sampler walk, then compaction IO) were
both wrong — exactly the "source-only Phase A picks the wrong attack
surface" anti-pattern.

An authoritative root-profile (root perf bypasses `perf_event_paranoid=3`;
non-root perf silently captured zero samples, which had masqueraded as
"off-CPU") nailed it: **74% of all CPU in `rowvalues.rs` `RowValues::
approx_bytes`, on-CPU.** The chain:

- `tier_tick` runs every 100 ms tick and feeds the tier's index/view
  memory floor via `index_runtime::reserved_bytes` → `Segment::stats()`.
- `Segment::stats()` maintained the scalar-postings byte total
  incrementally, but re-derived the `VALUES` side-channel term by calling
  `RowValues::approx_bytes()`, which **iterated every row** in the map.
- At 10M rows with a `VALUES` index that is a full-map scan (~50 ms) on
  the reactor every tick — a query landing in that window eats it. This
  matches every observation: tiering-only (`reserved_bytes` is only fed
  when tiering is on → non-tiered 10M had zero tail), `VALUES`-only (a
  no-index repro had none), O(rows)-scaling (3M ≈ 10 ms, 10M ≈ 55 ms),
  on-CPU, and it stalls even index-only `c4` (the stall is the shard
  tick, blocking the query fan-out).

**Fix:** `RowValues` keeps a running `heap` total, updated on every
`set`/`clear`, so `approx_bytes()` is O(1) — the same incremental pattern
the scalar postings already used. `Segment::stats()` now reads it. A
regression test asserts the counter equals a from-scratch rescan after
mixed insert/overwrite/clear.

---

## (superseded) Decomposition — the stall is inline vlog compaction

`kevy/src/commands.rs:261` runs `store.demote_step()` on every shard tick
(10 Hz). Cold point/row reads promote on second access → `used_memory` climbs
past the target → `demote_batch()` spills 32 records → and then, at
`kevy-store/src/tier_demote.rs:117`, calls `tier_compact()` **inline on the
reactor thread**:

- `tier_compact` → `kevy-vlog compact_below(50%, owner)` collects every sealed
  file under 50 % live and compacts **all of them in one synchronous call**.
- `compact_one` (kevy-vlog/src/lib.rs) walks the **entire** sealed file
  record-by-record: one `read_record` (pread) per record, one `append`
  (write) per surviving record, one `owner.moved` map-offset rewrite per
  survivor. At the 256 MiB rotate size and a 9.4 GB vlog, that is hundreds of
  thousands of syscalls **串在事件循环里** — tens to hundreds of ms per call.

Any query that lands while the reactor is inside `compact_one` waits the whole
rewrite. That is the ~56 ms wall. B6 is the same cause amplified: under
sustained 20 GB ingest the reactor spends most of its time compacting instead
of reading the socket, so load throughput collapses and the driver times out.

**This contradicts the arc's own design principle.** RFC §7 / D2 budget every
spill path — "a single write never funds an unbounded spill storm; continuation
rides the tick." Demotion is budgeted (`SPILL_BATCH = 32`). Compaction is the
one spill-family operation that was left **un-budgeted and synchronous** — an
implementation gap relative to the design, exposed only by the full-scale
Phase-B measurement (source-only Phase A did not size it — "Decomposition is
DISCOVERY not CONFIRMATION").

## Fix direction (orthodox, low-risk — matches the existing budgeted pattern)

Make compaction incremental and budgeted, mirroring demotion:

- Bound the work each `tier_compact` does to K records (a compaction cursor
  that remembers its position within the victim file), so per-tick compaction
  cost is microseconds, not the whole file.
- Continuation rides the tick, like `demote_step`. The bulk-load drain
  (`demote_to_watermark`) gets a matching compaction drain so ingest still
  reclaims space (guards vlog space amplification / B5) without starving the
  socket.

This keeps compaction **on** the reactor (no background-thread concurrency, no
new cold-value-correctness surface) — the same medicine that fixed the
demotion sampler. A background-thread compactor (using the pin/epoch/
`CompactOwner` machinery the RFC already built) stays available as a later
lever if budgeted-inline can't sustain B6 ingest, but it is higher blast radius
and not the first move.

## Release posture

The capacity arc ships in v4.0.0 as a headline feature. Its binding acceptance
criteria (C4/C5/B2/B6) fail at scale, so the irreversible release
(tag → crates.io publish) is held. The rest of v4 (embedded bench, client
packages, CI) is green. Branch CI on `feature/v4-capacity` is green after the
build fixes; only the T9 latency gate blocks.
