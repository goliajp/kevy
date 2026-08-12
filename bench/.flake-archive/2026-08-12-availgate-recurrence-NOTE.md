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
