# Pacing round 3: the pool's tenants, named — and neither is who the design served

Round 2 left three moves in decomposition order. All three ran; two of
them redrew the map.

## Move 1 — the B6 tenant is one 64 MiB mapping, not a churning batch

A park/take/reject histogram (temporary probe, printed from the reclaim
tick) over the whole B6 shape answered with **six park events total**:

```
65536K p=1 t=1 r=0 | 40K p=1 | 76K p=1 | 148K p=1 | 296K p=1 | 588K p=1
```

The pool is nearly *idle* during B6. Round 2's suspect — "the demote
batch path churns large buffers through the pool" — is refuted. The
split's 0.18× M3 cost was mostly **one 64 MiB mapping** (a generic
startup-scale structure: it shows up identically in a fresh pub/sub
server, so it is not a tier structure) whose free→retake window the
generation gate stretched across B6's RSS-peak sampling, plus a ~1.1 MB
tail of the 36 K–600 K ladder.

## Move 1a — under pub/sub the pool's occupancy policy was serving nobody

The same probe under the M2 shape (50 subs, size 64):

- **Steady state never touches the pool.** Zero park/take traffic while
  messages flow.
- At **connection teardown**, every conn frees its grown buffers at
  once: 100 frees of 52 KB (31 parked — filling every pool slot — 69
  rejected) and 100 frees of 100 KB (**all 100 rejected: the pool was
  already full of 52 KB entries**).
- Buffers grow monotonically through the ladder, so a ladder step's old
  size never recurs: the 31 parked 52 KB entries had **zero takes** —
  retention nobody would ever redeem, occupying the slots the churning
  length was refused from.

## Move 2 — the refault residual is gone; what remains is not pacing's

Sustained-load profiles (the first attempt sampled an idle server — the
200 k-msg bench finishes in under a second; redone at 20 M msgs):

| symbol | OFF (glibc) | ON (split) |
|---|---:|---:|
| `clear_page_erms` | 10.2 % | **9.3 %** |
| `rep_movs_alternative` | 13.9 % | 11.6 % |
| `deliver_publish` self | 14.6 % | **20.5 %** |
| allocator self (malloc+free / Heap::alloc+dealloc) | ~6.9 % | ~6.8 % |

R5's signature (OFF 12.2 % → ON 21.3 % zero-fill) has collapsed to
parity under the split: **the page-refault tax is closed**. The
remaining M2 gap (0.881 vs the 0.92 floor) sits inside
`deliver_publish` itself (+6 pp self-time), the header-free cache-line
cost of finding `2026-07-26-header-free-costs-a-cache-line.md`. That is
a layout question, not a WHEN-to-return question: **no pacing change
can buy the last 4 pp of M2.**

## Move 3 — sadd's tax hides in the tick path, same as hset's

sadd A/B profile: allocator self-time is par (ON 13.1 pp vs glibc
11.4 pp), but `drain_replica_inbox` reads 11.5 % ON vs 4.6 % OFF — the
+7 pp lives in tick-adjacent symbols, consistent with R4's finding that
the collection-write tax hides in the LTO symbol range around
`thread_reclaim`. sadd is not a separate mechanism; it is the same
reclaim tax, and its −17.1 vs −15.7 wobble across runs is box drift on
top of one cause.

## The first cut, and what its battery refuted

The probes' first reading produced two refusals in `pool_park`:
`POOL_MAX_LEN` = 1 MiB (giants pass through) and `POOL_PER_LEN` = 4 (a
dead ladder step cannot hog slots). The battery answered with three
corrections — two of them to *this arc's own earlier findings*:

1. **M3 did not move** (2.16×, RSS byte-for-byte with round 2's 737 MB)
   even with the 64 MiB entry and the ladder hoard both out of the
   pool. **Round 2's correction was itself wrong: the pool was never
   M3's 0.18× tenant.** The span side is byte-identical between the
   eager baseline and the split (diff: one blank line), so a three-way
   same-session run settled the rest: **eager (947a4660), split, and
   split+caps all read 2.16–2.17× today**, glibc 2.40× on every
   interleaved leg. The morning 1.98× does not reproduce; the split's
   "0.18× M3 cost" was a cross-session box-drift misread (the
   methodology's own "single run shows −X %" anti-pattern, cross-run
   variant). **Pacing has cost M3 nothing in any round.** The honest
   M3 criterion is parity with a same-session eager build — which every
   pacing variant meets — not "1.98× to the digit" against a number the
   box no longer produces.
2. **M2 fell to 0.863** (from 0.881). The per-length cap removed real
   value: the A/B runs four bench instances, and round 2's gain was
   teardown buffers parked whole and re-taken at the *next instance's*
   connection setup. Capped at 4 per length, 46 of each 50-conn wave
   paid mmap + zero-fill again.
3. **The zadd "wedge" is not a wedge and not the pacing's.** Isolated
   repro, two runs per build: round-2 build and caps build *each*
   stalled INFO once (>3 s) in one of two runs and *both* completed all
   four benches at ~3.6 M/s. The perfgate REFUSED verdicts of rounds 1
   and 3 (and round 2's green) are one coin flip over a
   build-independent >3 s pause under the 60 M-member zadd storm —
   a real tail defect, but a pre-existing one, and next-decomposition
   material (suspect: giant-structure growth pays alloc+copy+zero
   where glibc's realloc mremaps).

## The second cut, refuted in one leg

v2 dropped the per-length cap and widened `POOL_SLOTS` 32 → 128, sized
for the measured teardown burst. Its battery: **M2 recovered to 0.879**
(the cap's damage undone, round-2 level restored — and no further:
consistent with the profile's verdict that the rest is layout) — but
**one B6 leg answered RSS 883 MB / 2.59×** against glibc's 818, *worse
than no allocator*. The mechanism the extra slots enabled: dead entries
of **distinct** lengths — which exact-length matching can never serve
again (compaction-scale buffers vary) — pile up through the 64-drain
aging window; 32 slots capped that hoard structurally, 128 let it reach
the peak sampler. Surplus slots hoard corpses; the burst loss of small
slots is bounded, the hoard is not.

## The third cut (what this round ships)

Every number of this combination is same-session measured:

- **`POOL_SLOTS` = 32** (back) — the hoard bound, per v2's one-leg
  refutation.
- **`POOL_MAX_LEN` = 1 MiB** — parking a 64 MiB one-shot mapping buys
  one mmap and prices tens of MB of hysteresis-term retention; wrong
  trade at any measured frequency.
- **No per-length cap** — v1 measured its M2 cost (0.881 → 0.863) and
  the probe explained it (cross-instance teardown-buffer reuse).
- **64-drain aging stays** (round 2's surviving piece).
- Unit test pins burst-parks-whole, retake-serves-births, and
  giant-never-parks through the public alloc/dealloc lifecycle (the
  first version hand-rolled park calls and broke the accounting
  identity mid-test — parallel tests caught it).

## Round-3 battery

| line | round 2 (split) | caps v1 (per-len 4) | v2 (128 slots) | **v3 final (32 + 1 MiB cap)** | criterion |
|---|---:|---:|---:|---:|---|
| liveness | full run | zadd coin-flip (build-independent, see above) | — | **full run, all 12 angles** | full run |
| pubsub (M2) | 0.881 | 0.863 | 0.879 | 0.832 | ≥ 0.92 |
| M3 | 2.16× | 2.16× | 2.16× / **2.59× one leg** | **2.16–2.17×, 4/4 legs stable** | parity with same-session eager (re-anchored above) |
| sadd / hset / zadd | −17.1 / −13.1 / −14.3 | — | — | −15.8 / −12.0 / −14.4 (box drift ≤ 2.9 %) | ≥ 0.92 |
| lpush / kv / cluster | green | — | — | green (lpush −6.3, kv ≤ −5.8) | ≥ 0.92 |

**And a fourth correction, from the table itself:** across every pool
policy the M2 ratio reads 0.83–0.88 — eager 0.83–0.84, split 0.881, v1
0.863, v2 0.879, v3 0.832 — a ±0.05 cross-session band that swamps
every claimed pool effect. Round 2's "+4–5 pp from aging" does not
survive its own methodology: no pool variant demonstrably moves M2.
What *is* solid is the profile pair (zero-fill at parity, layout gap
+6 pp in `deliver_publish`): the M2 floor is a layout question with or
without pacing. The pool's third cut ships on structural grounds —
bounded retention (giant refusal + aging), zero measured cost on any
axis, all four M3 legs stable — not on a throughput claim.

## Where this leaves the design

- **M3 and liveness are settled**: pacing costs neither, in any round;
  both earlier "costs" dissolved under same-session controls.
- M2's remaining gap is the delivery path's memory layout
  (`deliver_publish` +6 pp self-time), outside the pacing RFC's scope
  and outside the pool's reach (fourth correction above) — its floor
  decision moves to the owner's table either way (accept 0.83–0.88
  M2, or open a layout attack face).
- The collection-write floor (hset/sadd/zadd) is the reclaim tick's
  price, which R4 measured as load-bearing; pacing rounds 1–3 have now
  spent the WHEN axis. Per the methodology, the next move on those
  angles is a fresh decomposition of the tick itself, not more pacing —
  with the zadd >3 s pause (and glibc's mremap advantage on
  giant-structure growth) as named entry points.
