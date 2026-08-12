# availgate crash-failover convergence timeout — ROOT-CAUSED & FIXED

- Occurrences: 2026-08-10 (first), 2026-08-12 run 31542402364 (second,
  log archived alongside). Fixed on
  `bugfix/replication-generation-identity` the same day.
- **Correction to this note's first draft**: the "dual-primary
  transient" reading was a misread of flattened forensics — n1's
  "this node is PRIMARY" line was its STARTUP self-election, not a
  split brain. The real defects (both source-proven):
  1. **Feed generations were counters, so distinct histories collide
     deterministically**: every fresh node called its history 1; n1's
     startup election bumped it to 2 and n2's failover promotion also
     bumped to 2 — a replica's stale cursor then passed the generation
     fence into offset aliasing.
  2. **The pump's caught-up check (`sent_offset >= primary_next`)
     shadowed the documented forked-history snapshot ship**: a
     same-generation cursor AHEAD of the feed sat "caught up" forever
     — heartbeats flowing, `master_link_status:up`, data never
     converging. That is the exact forensics signature (`postfail`
     missing for 60 s with io 0s ago and no resync in progress).
- Fix: generations are random 53-bit identities (RESP i64 + JS
  Number-precision safe) drawn at fresh boot / unclean boot / every
  bump; any mismatched cursor resyncs (identities carry no order);
  caught-up is exact equality, so an ahead cursor reaches the Future
  arm and ships a snapshot.
- Verification: new ahead-cursor e2e (wedges as ping-only on the old
  code) + feed/feed_meta identity unit tests + availgate ×10 on the
  box all PASS (earlier ×10 loop artifacts were the loop's own fixed-
  port AddrInUse collisions, cured with an inter-run settle).

## THIRD occurrence — post-fix (run 31581877633, log archived alongside)

The generation-identity fix landed two real defects but did NOT close
this flake: same wedge signature (survivor following the new primary,
link up, io 0s, no resync in progress, 60 s no convergence) with
random generations demonstrably live in the logs. The retargeted
fleet connected cleanly to all four of the new primary's feeds (no
connect errors after the follow line), so the remaining hole is past
the handshake — somewhere in adopt/ship/stream sequencing around the
promotion bump. Armchair analysis is exhausted: every traced
interleaving converges. Next step is a booked arc: INSTRUMENTED
reproduction — a temporary probe surface (per-runner cursor gen/
offset, per-shard feed gen/next, ship begin/end events) + an
availgate loop under CPU contention on the box until it fires with
the probes on. Box ×10 passed post-fix, so contention/timing is part
of the recipe.

## CLOSED — the third cause found (2026-08-12, `bugfix/availgate-promotion-window-repro`)

The probe surface went in and paid off, but not via the contention
loop: 28 contended rounds on the box did not reproduce it, because the
recipe is a ONE-TICK ordering window, not load. A deterministic
in-process reproduction closed it instead
(`promoted_node_ships_its_keyspace_to_a_fresh_cursor`).

Root cause: the generation fence had a fresh-cursor EXCEPTION. A
cursor claiming `generation 0 / offset 0` was adopted and streamed
from offset 0 — sound only while the feed's offset space covers the
store's whole life. A promotion bump replaces the source (frames
dropped, `next_offset` → 0) while the store keeps every key, so the
adopted cursor read as exactly caught up at 0 and was shipped nothing,
with heartbeats keeping the link up. A write accepted in the promotion
window (writes open on the epoch bump; each shard fences on its own
tick) lands in the old generation's backlog and is destroyed by the
bump — that is the `postfail` availgate then waits 60 s for.

Fix: every generation mismatch ships a snapshot, no exception — a
no-claim cursor is not proof of an empty replica (a respawned runner's
cursor always restarts at zero, so a replica holding a full stale
keyspace looks identical to a blank one on the wire). Full write-up:
`bench/FINDING-2026-08-12-availgate-promotion-window-wedge.md`.
