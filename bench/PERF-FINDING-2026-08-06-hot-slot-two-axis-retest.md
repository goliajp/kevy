# Hot-slot two-axis retest: negative on both axes — REVERT is the honest answer

**Verdict: `62797c6b` (the hot-slot layer) loses on both axes it was
built to win.** The three residual collection-write angles it targets
got *worse* by 4–5 pp each versus the per-word-claim round, lpush went
back under the floor, and M3 gave back a quarter of the arc's memory
win (1.98× → 2.067× against glibc's 2.40×). Per this train's own
discipline (the v2-J precedent: a chain the bench refutes is reverted,
not bisected into a polish round), the recommendation is to revert the
hot-slot layer and keep the per-word claim, which remains the residual's
only live mechanism.

Run on lx64 (kevybench, box idle, resident valkey untouched), branch
tip `62797c6b`, same-day interleaved A/B in both axes: OFF = plain
build, ON = `--features kevy-alloc`, same commit.

## Axis 1 — throughput (perfgate interleaved, 3 instances/angle)

| angle | per-word round (07-31) | + hot-slot (today) | Δ |
|---|---:|---:|---|
| legacy_8sh_sadd | −11.3 % | **−15.7 %** | −4.4 pp |
| legacy_8sh_hset | −12.5 % | **−17.0 %** | −4.5 pp |
| legacy_8sh_zadd | −13.0 % | **−18.4 %** | −5.4 pp |
| legacy_8sh_lpush | green | **−8.5 % (under floor by 0.5 %)** | red again |
| legacy_8sh_incr / set / get | — | −8.0 / −7.0 / −2.7 | pass |
| pinned (4 angles) | — | −2.6 … +0.8 | pass |
| zalg_zinterstore | — | **+9.9 %** | win holds |

Cross-day comparison is of same-day A/B *ratios*, the same discipline
the v8 ledger uses. M2 (pub/sub) measured 0.828–0.863 across three
runs — the v8 residual class, untouched by this layer, as its ledger
predicted. (One intermediate run printed 0.960 PASS; its OFF legs had
degraded to 18–19 M late in the run, so that pass was the reference
falling, not the candidate rising.)

## Axis 2 — M3 (hand probe, 2 M × 400 B on a 512 MB budget, one shard)

| build | used | RSS peak | resident/logical |
|---|---:|---:|---:|
| OFF (glibc) | 341.2 MB | 818.0 MB | **2.397×** |
| ON (tip, with hot-slot) | 341.2 MB | 705.3 MB | **2.067×** |

The probe validates itself: OFF reproduces the ledger's glibc 2.40× to
the digit, and both sides did identical logical work (used_memory equal
to the digit). The per-word round held ON at **1.98×** on this same
shape; the only delta between that commit and this tip is the hot-slot
layer. The arc's win over glibc was 0.42×; it is now 0.33×.

The failure mode is the LIFO cache's, in miniature: a cached slot stays
live from its span's view, so 32 pinned slots per (shard, class) hold
pages the densifier would otherwise return. The space bound kept the
wound to 0.09× instead of 0.40× — bounded, but on the wrong side of
zero, while buying negative throughput.

## The judgment gate has no envelope-scale carrier

The commit names hit/full counters as the RFC's judgment gate ("a cold
mechanism with no hits is a wrong hypothesis"), but `HotStats` is read
only by unit tests — nothing exports it from a running server, so this
retest cannot say whether the stack was cold or hot-but-harmful. Either
way the envelope numbers refute the layer; if a next design wants that
distinction, the counters need a surface first.

## Instrument notes (what it took to measure this)

- `allocgate-mem`'s runner still exits 1 before reporting (the known
  sequencing debt); the numbers above are the hand probe with identical
  arguments, per the per-word finding's own practice.
- perfgate's preflight (`pgrep -af "kevy|redis-benchmark"`) matches any
  *launcher* whose argv carries a `/home/kevybench/...` path — two
  refusals here were the gate seeing its own caller. Paths must travel
  in a script file's body, never on the launching argv.
- One real leftover was also caught and killed by pid: an Aug-04
  `primary_writer` on 7101, which had contaminated the first M2 sample.
