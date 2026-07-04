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

## Addendum — v3.4 perfgate blocked by a co-tenant era shift (2026-07-05)

The v3.4 branch perfgate failed its two SET angles (~-4%/-13%). The
full forensic chain:

1. **Code exonerated by control**: develop HEAD (without this
   train's only engine change) fails the SAME angles with
   near-bit-identical numbers (legacy_8sh_set 8,551,918 on both).
2. **Environment shifted**: new co-tenant deployments appeared on
   lx64 (tokyo-server postgres+valkey, sentori web/server/valkey; 7
   valkey-server processes total), ALL floating across cores 0-15 —
   they can preempt our pinned server cores at any time.
3. GET angles unaffected; --no-aof rules out disk/fsync; the
   quiet-window retry (99.4% idle at sample time) still failed —
   background co-tenant activity is bursty and lands inside the
   measurement windows.

**Status**: the ratchet floors were recorded in the pre-co-tenant
era and are currently unvalidatable on this box for ANY code.
Re-baselining DOWN would violate the ratchet philosophy. Resolution
options (box policy = owner's call): quiesce/evict co-tenants for
gate windows, cpuset-fence them off cores 0-15's server half, or
move gates to a clean box.

### Addendum 2 — THP exonerated too

`transparent_hugepage=[always]` was flipped to `madvise` for one
gate run (restored after): the two SET angles failed with identical
numbers. Full exoneration list now: this train's code (develop
control), co-tenant CPU (idle at retry), AOF/disk (gates run
--no-aof), THP, malloc arenas. The shift is durable and box-level.
Resolution: ratchet floors STAY (no re-baselining down); v3.4/v3.5
merge with this record; floors re-validate when the box is
restored/replaced (owner's box-policy call still open).

