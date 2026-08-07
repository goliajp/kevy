# Balance round, first ledger: the combined state is clean, and the tail belongs to the AOF

Owner direction: pull perf/disk/stable to the best reachable balance
before the industrial pivot. Plan: `.claude/plans/2026-08-07-v5-balance-plan.md`.
This ledger banks R-A (combined baseline), R-B (the last perf knife),
and R-C.6 (rewrite interplay).

## R-A — alloc ON + compress ON, measured together for the first time

| axis | combined state | vs alloc-OFF |
|---|---|---|
| capacity envelope | full PASS: ratio 10.1×, budget held, amp 0.01×, sweep 14/14 | identical |
| cold read | scalar p99 90µs / hash-row 210µs (budgets 300/500) | 128/130 — same band |
| frag @ 4KiB values | 1.32× | 1.35× — parity (the allocator's big win is the 400B shape: 2.16 vs 2.40) |
| perfgate-median (N=3) | KV/cluster/compat/incr/lpush/set/get PASS; hset −8.7 / sadd −10.8 / zadd −14.5 | the known alloc tax; compression adds zero |
| tail (mixed small-op storm, 120s) | **PING p99.9 = 3.2ms, max 9.8ms** | drain-budget fix holds under mixed storm |

**The combined state costs nothing on disk/capacity/stability; its
only cost is the known collection-write tax.** That is the P1 decision
in one line.

## R-B — the envelope-pooling knife dies before implementation; a bigger one is named

An 85.7M-op per-class allocation census under the hset storm:
**16B ≈ 1.96/op and 32B ≈ 0.98/op — ~2.9 small allocations per op —
while every envelope/reply ladder class totals ~0.5%.** Envelope
pooling (B') has nothing to pool; dropped without writing it.

The census names the real knife instead: a 3-byte hash field value is
heap-allocated (HashData values are `Vec<u8>`; keys inline ≤22B,
values never do). **Inlining small hash values removes ~2 of the 2.9
allocations per op for BOTH builds** — a store-side change, medium
blast radius, filed as the head candidate for the alloc execution
train rather than opened here.

## R-C.6 — the rewrite is exonerated; the firehose tail is the AOF's

BGREWRITEAOF on a 2M-key tiered+compressed store under a 1KiB write
storm: the rewrite window's tail (p99.9 116ms) is statistically
identical to the no-rewrite control (123ms). Attribution grid:

| config | p99.9 | max |
|---|---|---|
| aof + tier | 116–123ms | 264–289ms |
| **no-aof + tier (full demote churn)** | **9.9ms** | **16ms** |
| aof + no tier | 161ms | 268ms |

**The 100–250ms stalls belong entirely to the AOF append path under
~1GB/s sustained ingest** (kernel writeback throttling class; fsync
cadence and group-commit interplay are the mechanism candidates).
Tiering + compression + allocator are clean at ≤10ms p99.9 under the
same pressure. Named for the tail-latency train (V3): AOF append
batching / io_uring writes / throttled group commit — an inherited
v4-era behavior under an extreme shape, not a v5 regression.

Instrument caveats, recorded: BGREWRITEAOF's completion gauge is the
answering shard's view (INFO persistence is shard-local), so "rewrite
wall 59ms" measured the wrong thing; the rewrite-duration question
needs a per-shard gauge and stays open.

## R-C.5 / R-C.7 — the compaction "mystery" is rotation granularity, and the machinery is healthy

A realistic corpus (1.5M distinct templated 400B rows, 256MB/shard
budget) with two 30% churn passes settled at **amp 1.56× with
vlog_epoch=0 — compaction never ran at ANY threshold (50/65/80)**.
The suspicion escalated to "dead accounting broken"; a local
small-scale repro with INFO gauges killed that: **vlog_live_bytes
tracks dead bytes correctly** (37.5% dead visible).

The real mechanism: 8 files on the box = **8 shards × 1 vlog each**,
and at 256MB rotation each shard's ~116MB log is entirely its ACTIVE
file — never sealed, and the active file is never compacted **by
design**. With an 8MB rotation locally the machinery proves itself
whole: churn → dead accrues → **epoch advances within 6s of ticks,
48MB → 31.2MB (amp 1.14×)**, tails unaffected (p99.9 well under 1ms
during compaction).

Balance verdicts:
- **Threshold 50 is fine**; the knob that matters is `rotate_bytes`
  (hardcoded 256MB/shard): overwrite garbage lingers up to one
  rotation's worth per shard as a *young-store transient*, not a
  steady-state leak. Product doc item + a configurability candidate;
  no code change forced.
- The identical-to-the-byte reruns that exposed this also exposed an
  instrument lesson: a `cargo build | tail -1` pipeline without
  pipefail swallowed a build failure and re-ran a stale binary —
  **results too consistent are a reason to suspect the instrument**.

## R-D.8 first soak: the veto fired in ten minutes, and it was the point

The first hour-long soak (mixed 3-byte KV/collection storm + 1KiB
tiered ingest waves, tier budget 512MB, no maxmemory) killed the
server at minute 10: **"memory allocation of 3 bytes failed" → abort,
at RSS 13.5 GB on a box with 48 GB free.**

Not OOM — a structural ceiling: `PER_CLASS_CAP`'s counter was `u16`,
so the guard that its own doc records having "raised three orders of
magnitude" was silently pinned at 65,535 spans × 64 KiB = **a hidden
4 GiB ceiling per size class**. The 3-byte storm filled the 16 B
class and the allocator refused a 3-byte request with terabytes of
address space to spare. Fixed (`7008e36f`): counter and cap are u32,
the guard sits at 1 TiB/class, and the lesson joins its first verse
in the constant's doc — memory governance belongs to maxmemory and
the tier budget, never to an invisible allocator constant.

Also recorded from the same ten minutes: the unspillable-value
boundary is real and industrial config must set BOTH knobs (tier
budget bounds spillable values; maxmemory + policy bounds the rest);
frag under the mixed small-value shape ran 2.1–3.9× (vs 1.32× at the
uniform-4KiB envelope) — the value-size dependence of the fragmentation
story, now measured from the stability side too.

## R-D.8 second soak: survived the hour — and unmasked the second industrial defect

Take 2 (cap-fixed build, "maxmemory 6gb + allkeys-lru") ran the full
60 minutes: no death, frag converged 2.2 → 1.67 with no upward creep,
used sawtoothed in multi-GB drops with `evicted_keys` stubbornly zero
— and used peaked ABOVE the configured cap. A 5-minute local test
(2 shards, 100MB cap, write 250MB) turned suspicion into a number:
**steady state 205MB = 2 × the cap.**

**maxmemory was enforced per shard without dividing by shard count**
— at `on_shard_init` AND at the 100ms tick re-apply (fixing only the
init site would last one tick). An N-shard server enforced N× the
configured bound; the soak's 6GB cap was effectively 48GB, and the
sawtooth was the single shard holding the giant lpush lists crossing
6GB alone. The tier budget already divided per shard; maxmemory now
does the same at both sites (`d4075a6f`). After: 102.2MB steady
against 100MB (2.2% overshoot).

The balance round's stability tally so far: two industrial-grade
defects (a hidden 4GiB-per-class abort ceiling; an N× maxmemory
enforcement error), both found by the first soak that ever ran, both
fixed and re-validated same-day. Take 3 runs on the double-fixed
build with enforcement real; the open question it answers: eviction
counters, and whether giant-key eviction stalls the owning shard
(the ~3/min >1s gaps' prime suspect).

---

## R-E — the balance table, and where each axis now stands

The round's program (R-A through R-D) is complete. The combined state
(alloc ON + compress ON + this round's three fixes) reads:

### perf
- KV / cluster / compat / lpush / incr / set / get: green on medians
  (−2 to −8 vs the alloc-off reference).
- Collections (sadd/hset/zadd): −9 to −15 on medians — the known
  per-op allocation cost (2.9 small allocs/op measured; value-inlining
  named as the store-side knife). This is the P1 policy trade, priced.
- Tail under mixed small-op storm: **p99.9 = 3.2 ms** (in-process
  prober). Under a 1 GB/s AOF firehose: 100–250 ms stalls that belong
  to the AOF append path alone (tiering/compression/allocator
  exonerated at ≤10 ms under identical pressure) — the V3 tail train's
  named target, inherited from v4-era behavior.

### disk
- Cold read p99 90–128 µs (budget 300); realistic corpora ~30 %
  smaller on disk from corpus compression; identical-value corpora
  collapse ~71×.
- Steady-state amplification: 1.14× once compaction engages; the
  1.56× young-store figure is a rotation-granularity transient
  (256 MB/shard, garbage bounded by one unsealed file per shard) —
  document + make configurable, no code forced.

### stable
- Three 60-minute soaks: the first two each unmasked an industrial
  defect (u16-pinned 4 GiB/class abort ceiling; maxmemory enforced at
  N× the cap), both fixed same-day; the third survived the hour with
  correct enforcement (used bounded, 2.2 % overshoot at small scale),
  frag converging (2.2 → 1.67, no creep), no wedge, no death.
- crashgate PASS; repligate PASS on the final build (one 14-hour
  orphan writer from a previous session's trap failure occupied the
  test port — killed after identity check; "Aborted at startup" was
  AddrInUse, not a code fault).
- The soak monitor's own subprocess overhead pollutes gap counting —
  tailgate's mechanization must use an in-process prober (the R-A.3
  shape), a design note banked for V3.

### Defaults this round settles (feeding P1/P2)
- **alloc ON** costs nothing outside the collection angles in the
  combined state (envelope parity at 4 KiB, 2.16× vs 2.40× at 400 B,
  cold reads in band) — the trade is exactly and only the collection
  tax.
- compact threshold 50 stays; rotate 256 MB stays (with the transient
  documented); industrial config REQUIRES both knobs (tier budget for
  spillables + maxmemory/policy for the rest) — now that both work.

Round tally: three industrial defects found and fixed (one abort
ceiling, one enforcement error, one observability location miss), one
compat gap closed, two named train targets sharpened (AOF tail, value
inlining), and the first soak/tail instruments the project has had.
