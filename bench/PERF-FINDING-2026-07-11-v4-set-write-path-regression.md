# PERF-FINDING 2026-07-11 — feature/v4 SET write-path regression, caught by the K-402 gate re-record

> ## ⚠️ RETRACTED — 2026-07-12. There is no regression.
>
> Everything below is preserved as written, because the way it went wrong is
> the useful part. The verdict is withdrawn on two independent grounds:
>
> **1. The ruler was quantized.** The `legacy_8sh_*` angles pass `--threads`
> to redis-benchmark, whose only exit under `--threads` is its own 250 ms
> `showThroughput` timer (`redis-benchmark.c:52`, `:1653`). `totlatency` is
> therefore rounded UP to a multiple of 250 ms and the reported throughput is
> quantized to `N/(k·250ms)` — **7% buckets at this angle**. Convert the
> numbers in the table below: 9.21M → 3.2560 s, 8.56M → 3.5070 s, 7.49M →
> 4.0060 s. Every "disjoint distribution" in this document is a pair of
> adjacent buckets. So is the "8.55M attractor" the caveat below worries
> about — the doc looked straight at the grid and theorised about the system.
>
> **2. The A/B order was fixed.** Every round ran v3.18.0's three instances
> first and feature/v4's second. The box slows monotonically across a long
> run, so whichever binary goes first is systematically favoured. That alone
> is enough to manufacture the separation, and it explains the pinned angles
> too — those do NOT use `--threads` and carry no quantization.
>
> **Re-measured 2026-07-12, order-balanced, ruler fixed** (throughput read
> from the server's own command counter over a timed window):
>
> | angle | v3.18.0 | feature/v4 | Δ |
> |---|---:|---:|---:|
> | legacy_8sh_set (steady-state) | 5,989,391 | 5,975,577 | **−0.23%** |
> | legacy_8sh_get (steady-state) | 7,411,561 | 7,483,891 | **+0.98%** |
> | legacy_8sh_set (n≈44 instances, quantized ruler, medians) | 8,556,796 | 8,554,357 | **−0.03%** |
> | pinned_cluster_set (no `--threads`, order-balanced) | 22,238,775 | 22,016,850 | **−1.0%** |
>
> All within sample variance (3.5–5.6%). The claimed −18.7% / −7.5% / −5.6%
> do not exist. The two-step bisect below (T1a/K-108 then K-110) was fitting a
> mechanism to an artefact — and note that the source-level search for that
> mechanism found nothing: `Agg`, `PendingSlot`, `Route`, `Op` and `Inbound`
> are byte-identical across the two commits, and `Store::set` still takes an
> owned `Vec<u8>`.
>
> Full account: `bench/PERF-FINDING-2026-07-12-benchmark-250ms-quantization.md`.
> Rules added: R12 (a clean A/B needs the right control AND a balanced order),
> R13 (readings on evenly-spaced levels → suspect the ruler first).

**Context**: v4 T4 K-402 (re-record the stale legacy_8sh baseline pair +
add the five arena angles to the perfgate ratchet). The full 12-angle
measure on `feature/v4` @ `f7585650` surfaced three SET angles far under
the 2026-07-03 baseline. Per the Pre-Phase-A gate (verify a gap is real
before believing it), an A/B against the v3.18.0 tag — same box, same
hour, 3 fresh instances per angle — was run before touching any number.

## A/B matrix (lx64, 2026-07-11, N=30M, 3 fresh instances each)

| angle | v3.18.0 (ae466400) | feature/v4 (f7585650) | Δ (v4 vs v3.18) |
|---|---|---|---|
| pinned_cluster_set | [22.51 · 22.68 · 22.52M] → 22.52M | [19.75 · 20.92 · 20.82M] → 20.82M | **-7.5%, distributions disjoint** |
| pinned_compat_set | [17.00 · 16.66 · 17.41M] → 17.00M | [16.05 · 16.38 · 15.94M] → 16.05M | **-5.6%, distributions disjoint** |
| legacy_8sh_set | [9.21 · 9.21 · 9.21M] → 9.21M | [7.49 · 7.98 · 7.49M] → 7.49M | **-18.7%, distributions disjoint** |
| pinned_cluster_get | (baseline 30.40M) | [30.16 · 30.14 · 29.99M] → 30.14M | -0.9%, within tolerance |
| pinned_compat_get | (baseline 19.39M) | [19.03 · 18.86 · 21.04M] → 19.03M | -1.9%, within tolerance |
| legacy_8sh_get | [10.88 · 10.89 · 9.97M] → 10.88M | [10.88 · 10.87 · 10.89M] → 10.88M | 0.0% |

Reading:

1. **Every SET angle regresses, every GET angle holds** — this is a
   write-path property of the branch, not box noise. All three SET
   comparisons have zero overlap between the two binaries' instance
   draws (v4's best < v3.18's worst), on the same box in the same hour.
2. **The arena protocol does not see it**: K-401's arena run on the
   same binary scored SET 6.39M — byte-identical to the v3.18.0 ledger
   row. The regression only expresses at deep pipelining (perfgate's
   P256 vs the arena's P16), i.e. it costs per-op work that pipeline
   overlap can no longer hide at high depth.
3. **Box drift exists but is smaller and separate**: v3.18.0 itself
   measures below the 2026-07-03 baseline on the pinned pair
   (22.52M vs 24.31M recorded, -7.4%; 17.00M vs 17.52M, -3.0%) and on
   legacy set (9.21M vs 9.97M, -7.7% — today it sits cleanly on the
   9.2M attractor documented in the 2026-07-03 bimodal finding). That
   part is environment/history, not v4 code.

## Suspect range

`ae466400..f7585650` contains three real-code blocks: the T1a runtime
instantiation (K-104 W1–W6: process-globals → `RuntimeState`,
thread-locals → `ShardCtx`, hot-path role flags → shard-local cache +
epoch invalidation), the K-108 API break (store write surface to
`&[u8]`), and the K-110 kernel verdicts (R1–R4). A mid-point probe at
`8910ba84` (= post-T1a/K-108, pre-K-110) on the legacy_8sh_set angle,
same box, same hour:

| binary | legacy_8sh_set instances | median |
|---|---|---|
| v3.18.0 (ae466400) | [9,213,798 · 9,210,970 · 9,208,143] | 9,210,970 |
| 8910ba84 (post-T1a/K-108, pre-K-110) | [8,559,237 · 8,556,796 · 8,556,796] | **8,556,796** |
| f7585650 (HEAD) | [7,488,798 · 7,978,758 · 7,485,062] | 7,488,798 |

**The regression lands in two steps.** T1a/K-108
(`83fb958e..8910ba84`) costs -7.1% (9.21M → 8.56M) and the K-110
kernel verdicts (`cc2c8bef`) cost a further -12.5% (8.56M → 7.49M).
Each binary pins tightly to its own level (instance spreads < 0.1%,
< 0.1%, ~7%) with zero overlap between levels. Caveat for the decomp:
8.56M coincides with the historical "8.55M attractor" from the
2026-07-03 bimodal study, so the mode-attractor structure may be
interacting with the code change — but three binaries separating
cleanly in the same hour is a code signal, not a draw. Note the
sandwich lesson: T1a's own perfgate sandwich (0ca4f254) validated
adjacent commits; a few-percent cost per wave compounds and only the
cross-tag A/B shows the sum.

## Baseline handling (ratchet discipline)

- The five new angles (INCR/SADD/HSET/LPUSH/ZADD) enter the ratchet at
  the branch's measured values — they have no prior history; when the
  regression is fixed the next `--update-baseline` lifts them.
- `legacy_8sh_get` re-records at 10,877,494 (bit-identical to the old
  record; this angle is a rock).
- `legacy_8sh_set` re-records at **9,210,970 = v3.18.0's same-hour
  median**, NOT the branch's 7.49M — recording the regressed value
  would launder the regression out of the gate. The re-record still
  fulfils its purpose (the stale 9.97M bimodal-era number is gone;
  today's floor sits under the 9.2M attractor instead of inside the
  mode mixture).
- The pinned four + zalg keep their 2026-07-03 records untouched
  (no re-record was authorized for them, and lowering a SET floor to
  green the gate is exactly what the ratchet forbids).
- Consequence: the 12-angle gate on feature/v4 is **9 green / 3 red**
  (pinned_cluster_set, pinned_compat_set, legacy_8sh_set) until the
  write-path regression is found and fixed. The red is the finding.

## Phase A decomposition (same day, later hours — perf record + counters + codegen diff)

Three `release-perf` builds (v3.18.0 / 8910ba84 / HEAD f7585650) from
read-only worktrees, same box, interleaved rotation throughout.

### 1. The headline steps do not reproduce — the mode mixture does

Interleaved legacy_8sh_set, 10 draws per binary (one rotation sweep),
then 8 more draws per binary in the fix-validation battery. Every
binary samples the SAME attractor set {≈7.5–8.0M, ≈8.55M, ≈9.21M}:

| binary | high-mode 9.21M draws | 8.55M draws | low draws |
|---|---|---|---|
| v3.18.0 | 7/21 | 12/21 | 2× 7.98M |
| 8910ba84 | 7/13 | 6/13 | — |
| HEAD | 5/21 | 15/21 | 1× 7.53M |

HEAD reaches 9.21M repeatedly (rounds 4/6/7/9 of the sweep; 3/8 of the
battery). The morning A/B's "-18.7%, distributions disjoint" was a
draw-correlation artifact of that hour (an ollama inference job was
resident on the box during re-measurement; the 7.49M attractor never
appeared again in 21 HEAD draws). The 2026-07-03 bimodal finding's
mode-attractor structure is the dominant term, not a code step.
Per-instance AnonHugePages was flat (~512MB, fully huge) across all
draws — THP luck is NOT the mode discriminator; the mode axis remains
unexplained (open item, unchanged since 2026-07-03).

### 2. What IS code-real: a small diffuse cost, and one codegen flip

- perf record symbol composition is flat across all three binaries
  (run_uring 38.4–39.8%, every hot symbol within ±0.7pp) — no
  localized regression symbol exists. §9 gate: no attack target
  ≥ 10pp; the cost is diffuse.
- perf stat instruction counters first read HEAD ≈ +2.3%/op vs v3.18.0
  in same-mode runs (K-110 ≈ +1.9pp, T1a ≈ +0.4pp); a later repeat
  scrambled within ±2% intra-binary variance (busy-poll spin pollutes
  the counter). Signal: suggestive, not proven.
- **Codegen decomposition (nm -S, the hard fact)**: K-110 flipped
  LLVM's inline cost model on the per-op hot path. At 8910
  `start_multi_or_crossslot<ArgvBorrowed>` (the whole multi-key /
  CROSSSLOT orchestrator, 18.0KB) is a standalone symbol; at HEAD it
  is fully inlined into `Shard::start_command` — the per-op route
  dispatcher — bloating it 30,665 → **43,260 bytes (+41%)**, degrading
  its register allocation and I-cache/iTLB locality (HEAD's
  dTLB-load-misses ran 2–3× the v3.18 binary's in the counter runs).
  `exec_op` simultaneously shrank 42,742 → 36,221 (inliner reshuffle,
  fat LTO + 1 CGU). T1a's growth was benign (+0.6%
  `start_command`, +4% `dispatch_with_proto`).
- pinned_cluster_set (tight, unimodal angle) confirms a small real
  cost: interleaved medians v3.18.0 22.73M / HEAD 22.45M = **-1.25%**
  (non-overlapping draws) — not the morning's -7.5%.

## The fix (v4 K-110 codegen restore — one attribute)

`crates/kevy-rt/src/exec_crossslot.rs`: `#[inline(never)]` on
`start_multi_or_crossslot` (start_command's cold catch-all arm; SET /
GET / all single-key traffic never enters it). Restores the pre-K-110
hot-path shape: `start_command` back to 30,567B, the orchestrator
standalone again. Zero semantic change; multi-key commands pay one
call. Architecture (R1–R4 verdicts, K-108 API, T1a instance) untouched.

### Phase B numbers (same box, interleaved, release-perf)

| angle | v3.18.0 | HEAD (pre-fix) | HEAD+fix | floor (baseline×0.92) |
|---|---|---|---|---|
| legacy_8sh_set (8 draws) | 3×9.21M · 5×8.55M | 3×9.21M · 4×8.55M · **1×7.53M** | 4×9.21M · 4×8.55M, min 8.55M | 8.47M — fix: all draws above; HEAD dipped below once |
| pinned_cluster_set (3 draws) | 22.71–22.75M, med 22.73M | 22.34–22.50M, med 22.45M | 22.51–22.53M, med **22.51M** | 22.37M — fix: all above; HEAD dipped below once |
| legacy_8sh_get (3 draws) | 9.98 · 10.87 · 10.89M | (morning: 10.88M rock) | 9.98 · 9.97 · 10.89M — same mode set | 10.01M×? — GET bimodal per 2026-07-03, unchanged |
| pinned_cluster_get (2 draws) | 30.15 · 30.47M | (morning -0.9%) | 30.07 · 30.08M (-0.3%, within tolerance) | 27.97M ✓ |

The fix recovers the codegen flip (~+0.3% pinned, the worst legacy
attractor not re-drawn); the residual -0.95% pinned vs v3.18.0 is the
diffuse T1a+K-110 instance/gate tax — real, small, and priced into the
architecture the v4 kernel verdicts bought. Workspace tests green +
clippy 0 on a clean f7585650 + fix worktree.

### Ratchet consequence

With the fix, the branch sits above every SET floor in this box state;
the morning's three red angles were mode-mixture draws taken in a
polluted hour stacked on the (now fixed) codegen flip. The
legacy_8sh_set re-record at 9,210,970 stands (it is the high attractor,
drawn by ALL binaries including HEAD+fix). The pinned records stand
untouched. Gate verdict moves to: re-run perfgate on a clean hour with
the fix landed; expect green or red-by-draw — a red there is the
2026-07-03 mode-instability open item, not this regression.
