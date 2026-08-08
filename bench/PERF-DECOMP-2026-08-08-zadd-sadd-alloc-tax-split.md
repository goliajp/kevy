# zadd/sadd alloc-ON residual — Phase A decomposition (tax split)

Question: after the hash angle recovered (V4 value-inline), P2's two remaining
reds were sadd −9.7 % and zadd −13.5 % (perfgate-median n=5, vs the recorded
ref). Are those the kevy-alloc fast-path tax, and is there a concentrated seat
worth a Phase B (the fastpath-residue option C)?

## Method

lx64, develop tip `14883742`, two binaries from the same source (alloc-ON =
`--features kevy-alloc`, OFF = default/glibc), 2-shard uring server on cores
0-1, client cores 8-11, `-P 256` (CPU-bound cell — P=1 cells structurally
cannot price per-op alloc cost).

1. **Owner-thread perf sampling** (all threads sampled, per-thread split —
   `ps` TIME cannot tell the busy-poll idle shard from the owner; take 2 of
   this round sampled the idle shard and learned that lesson).
2. **Same-box ON vs OFF throughput A/B** (median-of-5, N=30M), splitting the
   "vs ref" number into (alloc tax = ON vs OFF now) + (baseline distance =
   OFF vs recorded ref).

## Owner-thread profiles (zadd, per-thread, self-time)

| symbol | ON | OFF |
|---|---|---|
| slice binary_search<(Score, SmallBytes)> | 17.02 % | 17.02 % |
| ZSetData::insert | 5.85 % | 5.97 % |
| ranktree insert_rec | 2.76 % | 2.76 % |
| ranktree remove::fatten_child | 2.10 % | 2.46 % |
| kevy_alloc Heap::alloc | 2.19 % | — |
| KevyMap find_by_borrow | <2 % | 2.19 % |

The two profiles are symbol-for-symbol nearly identical; the allocator's
distinct symbols are ~2-4 % on ON vs <2 % glibc on OFF. **§9 gate verdict: no
≥10 pp seat** — a 1-3 pp self-time delta cannot explain a 13.5 % throughput
gap, which pointed at the vs-ref comparison itself.

## Same-box A/B (median-of-5, P=256)

| op | OFF | ON | true alloc tax |
|---|---|---|---|
| zadd | 1,178,787 | 1,140,603 | **−3.2 %** |
| sadd | 4,810,026 | 4,014,469 | **−16.5 %** |

## Conclusions

1. **zadd's −13.5 % red is NOT an alloc problem.** True alloc tax −3.2 %;
   zadd is tree-walk-bound (binary_search 17 % + tree ops ~13 %; B-tree
   allocation amortises to <0.1 alloc/op at 15 keys/node), so per-call
   allocator cost barely registers. The remaining ~10 pp vs ref is baseline
   distance — consistent with the n=5 board showing PASS angles broadly at
   −5~−8 where the 2026-08-08-morning verification account had −2.9~+3.3.
   **zadd drops out of the fastpath-residue option-C scope.**
2. **sadd is the one real alloc-tax angle: −16.5 % same-box.** It reconciles
   with the pacing arc's fast-path pricing (~1.7×/call): at 208 ns/op ×
   ~2 alloc calls × ~15-20 ns per-call delta ≈ 15-20 %. Diffuse per-call
   cost, no dominant seat (the pacing arc's owner-thread topdown already
   peeled this four layers deep). Option C therefore means fast-path/class
   redesign sized against ONE angle, not two.
3. **A separate non-alloc question is now on the table**: the board-wide
   ~4-5 pp sag of develop vs the recorded ref (all angles, alloc irrelevant).
   Box drift and develop-side regression are indistinguishable from the n=5
   run alone (cross-session noise-band lesson); deciding it needs a same-box
   A/B of develop-tip vs the last-release tag build. Filed as its own
   follow-up, not part of the alloc decision.

## Input to the P1/P2 decision (final form)

- alloc default-ON's real cost today: **sadd −16.5 %** (diffuse fast-path
  per-call), zadd −3.2 %, hset −0.2 % (V4 recovered), everything else priced
  green earlier.
- Options: **A** accept (alloc stays opt-in) or **C** fast-path/class
  redesign scoped to the set fast path. B (widen claims 1-3 %) remains
  insufficient. Owner's call.

## Measurement lessons (this round)

- `ps` TIME cannot identify the owner among busy-poll shards — sample all
  threads and split per-thread in the report.
- fp call-graphs cannot split the LTO `run_uring` aggregate, and srcline
  needs debug line info a release build lacks; per-thread symbol split was
  sufficient here.
- A "vs recorded ref" red is a sum of (true A/B delta) + (baseline drift);
  before sizing an attack against it, split it with a same-box A/B.
