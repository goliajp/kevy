# v3.4 tails closure — 286c4a2 micro-mechanism + epoll stay-hot symmetry

## Case 1 — 286c4a2 "-4%" micro-mechanism: CLOSED (cost no longer exists)

The v2.2-era campaign attributed a -4% step to 286c4a2 (INFO
whole-process aggregation: per-command thread-local counter increment
+ O(1) `expires` accounting on TTL transitions).

A/B on today's code (lx64, isolated, GET/SET 8M × median-of-5):
build B no-ops BOTH hot-path hooks (`on_command` body + 
`adjust_expires` body).

| build | GET | SET |
|---|---|---|
| A (current) | 6,394,884 ±3.6k | 6,389,776 ±476k |
| B (hooks no-op'd) | 6,389,776 ±2.3k | 6,389,776 ±475k |

Delta < 0.1% — far inside the noise band. **Mechanism verdict**: the
accounting instructions themselves cost nothing measurable today; the
era-sweep's -4% was a layout/codegen perturbation of that commit's
struct growth in the v1.17 code base (or era-sweep noise), long since
absorbed by subsequent evolution. Nothing to attack; case closed.

## Case 2 — epoll stay-hot symmetry: FIXED

The v2.2 stay-hot-while-inflight fix (uring reactor, 65b7515) held
the spin rung while cross-shard replies were outstanding. The epoll
shard loop lacked the clause: it parked mid-conversation and paid a
kernel wake per reply batch. `shard.rs` idle ladder now resets
`idle_spins` while `xshard_inflight > 0` — same bounded-by-drain
rationale, loom-covered park/wake fences unchanged.

## Case 3 — IDX.QUERY conn-tail: see PERF-FINDING-2026-07-04-idxquery-conn-tail.md (open, next)
