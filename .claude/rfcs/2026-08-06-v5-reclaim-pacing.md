# RFC · reclaim pacing — one knob, both residuals

> v5 experiment arc, alloc train. **Status: DESIGN — awaiting approval,
> zero implementation.** Every input below is measured, not assumed;
> the two findings it rests on are
> `bench/PERF-DECOMP-2026-08-06-collection-write-residual.md` and
> `bench/PERF-FINDING-2026-08-06-pubsub-residual-is-page-refault.md`.
> Per arc law ⑤, any premise here dies by measurement, not by argument.

## 1. The measured problem — two taxes, one policy

After the per-word claim, the allocator still loses **−11…−13 %** on
sadd/hset/zadd and **~0.86** on pub/sub against the 0.92 floor, while
holding M3 at **1.98× vs glibc 2.40×**. Decomposition located both
residuals in the *same* place:

- **Write side (R4):** the named alloc path costs only ~3 pp more than
  glibc; the larger +9–12 pp rides the per-tick `thread_reclaim()`
  sweep, inlined into the reactor tick. Removing the sweep is not an
  option: a no-reclaim build **wedges** partway through perfgate's
  angle sequence (~300 M accumulated ops), twice reproduced — reclaim
  is load-bearing for liveness, not just for M3.
- **Kernel side (R5):** `clear_page_erms` — zero-fill on first touch of
  a returned-then-refaulted page — nearly doubles under the allocator
  (12.2 % → 21.3 %, the top symbol under pub/sub). glibc never returns
  pages, so it never pays re-entry.

The current policy is **maximal eagerness** (`reclaim.rs`): page-granular
return every tick, large retained mappings returned every tick
("retention beyond a tick requires sustained traffic to re-earn"), and
hysteresis covering only whole empty spans, per sweep. Every page freed
this tick goes to the kernel this tick — and comes back through
`clear_page_erms` next burst.

## 2. The design claim

The two taxes are the price of *when* pages return, not *whether* they
can. M3 does not require eager return — it requires **bounded** free
memory (the accounting's `hysteresis` term is already defined as
"empty spans deliberately kept, O(low-water policy), an explicit
bounded knob"). So the design space is the pacing policy, with the
retained bytes carried under the existing `hysteresis` accounting term
— no new term, no unexplained residual.

## 3. Candidates

| # | shape | write-side tax | kernel-side tax | M3 shape | reference |
|---|---|---|---|---|---|
| **A. age-gated pages (decay)** | a page must be free for ≥ T ticks before returning | sweep still walks every tick, cost similar | refault only after true idleness — burst cycles stop paying | retained ≤ churn-window working set; decays to today's floor when idle | jemalloc decay (already in the RFC 9 reference table) |
| **B. per-tick return budget** | return ≤ K pages/tick | sweep can early-exit at K — tick cost bounded | thrash reduced but hot pages can still return under budget | slower convergence to floor under big frees | — |
| **C. every-N-tick sweep** | sweep runs 1/N of ticks — write-side tax ÷ N directly | unchanged per sweep; burst-cycle refault persists if N·tick < burst period | steps toward floor every N ticks | trivial | — |
| **D. watermark-driven** | sweep only when `span_free` exceeds X % of mapped | zero cost in steady state below watermark | idle memory can sit indefinitely below X | floor = X % above live — a *ratio* term, scales with data | Go runtime scavenger (goal-percent) |

**Leaning: A, possibly A+B.** A is the only candidate that addresses
*both* taxes by construction (C halves the sweep but not the refault;
B bounds the tick but not the thrash; D converts the floor into a
scaling term, which §8.1's "only rounding may scale" rule forbids).
A's retained set is bounded by the churn window (idle pages older than
T still leave), so the M3 scaling statement survives. B composes with A
as a tick-latency bound if the sweep's walk itself proves expensive at
scale. This leaning is a hypothesis for the A/B to kill, not a
decision.

## 4. The liveness bound (non-negotiable)

The NR wedge shows an un-reclaimed heap eventually stops serving — the
mechanism is unconfirmed (span-list growth, cap exhaustion, or
something else), so the bound must not depend on knowing it: **every
free page returns within a bounded number of ticks under every policy**
(A: T ticks after going idle; B: total-free ÷ K ticks; C: N ticks).
"Below the watermark forever" (D alone) violates this and is another
reason D is disfavoured except in composition.

## 5. Acceptance (numbers already carried by instruments in place)

| criterion | carrier | today | target |
|---|---|---|---|
| collection angles converge | perfgate A/B (M1) | hset −12.5 / sadd −11.3 / zadd −13.0 | ≥ 0.92 floor |
| pub/sub converges | allocgate M2 | 0.83–0.86 | ≥ 0.92 |
| refault share drops | `perf record`, `clear_page_erms` self-time | 21.3 % (ON) vs 12.2 % (OFF) | → OFF's share |
| M3 holds | allocgate-mem (runner fixed, `105eecc3`) | 1.98× | ≤ 1.98× to the digit |
| liveness | full perfgate angle sequence on one instance | NR wedges | no wedge, all angles measured |
| accounting balances | M3-identity line | EXACT | EXACT, retained bytes under `hysteresis` |

Per arc law ①, the targets above are consequences to verify, not the
design statement. The design statement is: *after pacing, the bytes
between logical and resident consist of the same four terms as §8.1,
with `hysteresis` enlarged by an explicit, bounded, decaying retention
set — and nothing else changed.*

## 6. Decision points (owner)

1. **Approve the design round at all?** The alternative on the table is
   accepting −11…−13 % on collection writes as the price of −17 %
   resident (the SME trade named in the v8 ledger).
2. **Candidate A (decay) as the primary shape?** §3's leaning, held
   loosely.
3. **T's unit**: ticks (simple, load-sensitive) vs wall-clock
   (jemalloc's choice, steadier under variable tick rates).
4. Whether the hot-slot layer (`62797c6b`, two-axis negative, revert
   recommended in its finding) is reverted before or with this work —
   pacing changes the ground it would be re-measured on either way.
