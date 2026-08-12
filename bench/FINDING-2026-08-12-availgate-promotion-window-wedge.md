# FINDING 2026-08-12 — the availgate failover wedge: a no-claim cursor was never shipped anything

Branch `bugfix/availgate-promotion-window-repro`. Closes the flake
archived three times (`bench/.flake-archive/2026-08-12-availgate-*`),
whose signature never changed: the surviving replica follows the new
primary, connects to all four per-shard feeds, `master_link_status:up`,
`master_last_io_seconds_ago:0`, `master_sync_in_progress:0`, and 60 s
later the post-failover write still is not there.

P4 fixed two real defects on the way here (generations became random
53-bit identities instead of colliding counters; the caught-up check
became exact equality so an AHEAD cursor reaches the snapshot arm).
Both were necessary. Neither was this one, which is why the flake came
back a third time with random generations demonstrably live in the log.

## Mechanism

Two assumptions met and cancelled each other out.

**The pump's fresh-cursor exception.** A cursor presenting
`generation 0 / offset 0` was read as "fresh replica, no continuity
claim, nothing served yet": adopt the feed's generation, stream from
offset 0. That reconstructs a replica only while the feed's offset
space has covered the store's whole life.

**Generation bumps break exactly that.** `bump_generation` REPLACES
the source — every buffered frame dropped, `next_offset` back to 0 —
while the store keeps every key. Promotion does it
(`shard_tick.rs:apply_promotion_epoch`), and so does an unclean boot
with data on disk (`kevy_persist::feed_meta` boot table, the
`(Some(g), _)` arm). Afterwards the adopted cursor sits at
`sent_offset == primary_next == 0`, which the caught-up check reads as
exactly caught up, and the pump returns. Nothing is ever shipped. The
1 Hz heartbeat is appended unconditionally for every streaming conn —
outside the did-work gate on purpose — so the link looks perfectly
healthy while the keyspaces diverge. Both silent arms of
`fill_streaming_output` (the adopt, and the caught-up early return)
were also the only two arms in that file with no log line.

**Why availgate usually passes.** The promotion counter bumps on the
elect thread and writes open at once, but each shard fences on its own
tick (~100 ms). If `SET postfail` lands AFTER its shard's bump, it is
offset 0 of the new generation, the retargeting replica's 0/0 cursor
falls through to `frames_from(0)`, and the frame is served — green. If
it lands INSIDE the window, it is appended to the old generation's
backlog and the bump destroys it: the write survives in the store but
no longer exists in any stream, and the 0/0 cursor is told it is caught
up. Green or wedged is decided by which side of one tick the write
falls on, which is why it only ever fired on a contended hosted runner.

**A no-claim cursor is not an empty replica.** A runner's cursor lives
in its thread (`replica_runner.rs` `from_offset` / `data_gen`) and
restarts at zero on every respawn — and elect retarget always respawns
(`state/replication.rs::start_runners` stops and rebuilds the fleet).
So a replica holding a full stale keyspace presents on the wire exactly
as a blank one. Frames only add and overwrite; streaming can never
remove what the replica holds and the primary does not. Only a snapshot
replaces a keyspace.

## The fix

The generation fence loses its fresh-cursor exception: **every**
mismatch ships a snapshot. One branch deleted, one contract restored —
and the same contract Redis has always had, where a replica with no
continuity gets a full resync rather than a replay from genesis.

Cost: a fresh join now pays one snapshot instead of replaying the
entire history from offset 0. For a store built by N writes the
snapshot is never larger than the replay it replaces, and for an
overwrite-heavy history it is dramatically smaller.

## Reproduction (deterministic, in-process)

`crates/kevy/tests/replication.rs::promoted_node_ships_its_keyspace_to_a_fresh_cursor`.
A node follows a primary, mirrors 50 keys, takes `REPLICAOF NO ONE`
(the same promotion counter an election win drives), takes a write in
the window, and then a fresh cursor attaches. Pre-fix it hits one of
two failures, both this defect: ten heartbeats and no snapshot (the
availgate stall), or a single post-bump frame with the 50 mirrored keys
missing (silent divergence — the more dangerous face). Post-fix:
snapshot.

Getting there took two wrong reproductions worth recording. The first
promoted a node that had never been a replica —
`promote_stop_runners` bumps only `if was_replica`, so nothing fenced
at all, and the probe surface said so immediately (`next 51`, no
`promoted` line). The second promoted before the shard's first tick,
which LATCHES the first epoch it observes without acting, so again no
bump. Both were visible only because the probes print the feed's
position next to the cursor's.

## Verification

- New e2e above: RED (both faces) → GREEN.
- `cargo test -p kevy --test replication` 26/26; kevy-rt / kevy-replicate
  / kevy-persist suites green.
- **`bench/upgrade-interop.sh` scenario C**, mixed-version against the
  released v5.0.0 binary: a replica carrying a 5.0 counter-generation
  sidecar, retargeted at a 5.1 primary, was **RED before this change**
  (it kept its pre-upgrade keys — the fork survived) and now resyncs
  via exactly one snapshot ship per shard, fork discarded. A/B (fresh
  join both directions) and D (AOF/vlog dir round-trip old→new→old with
  vlog-sized values) pass. So this fix is what makes the documented
  5.0 → 5.1 upgrade self-heal, not only what closes the flake.
- **Box availgate under contention** (`bench/availgate-loop.sh`, 32
  contenders on 16 cores): the pre-fix probe build ran 28 rounds
  without reproducing — the window is one tick wide, so contention
  alone is a poor trigger and the deterministic in-process
  reproduction is what closed the case. The fixed build's loop is the
  regression run.

## Probe surface (kept)

`KEVY_DEBUG_REPL_TRACE=1` lights per-shard feed position vs every
attached cursor's claim: handshake claims, the promotion bump's
pre-bump state with all attached conns, streaming cursor vs feed at
1 Hz, ship begin/end, AckSent-stuck detection, and the runner's session
start / snapshot window / first frame. Every line carries a wall-clock
ms stamp so three node logs interleave into one sequence. Cost when off
is one cached boolean; the modules are `kevy-rt/src/repl_trace.rs`,
`kevy-rt/src/replication_trace.rs`, `kevy/src/replica_trace.rs`. It
paid for itself twice inside one session and is kept for the next
field question.

`bench/availgate.sh` also learns `KEVY_AVAILGATE_KEEP=<dir>`, which
copies the run directory out before the EXIT trap destroys it — the
gate used to delete its own crime scene at exactly the moment it
mattered.

## Lessons

- **The two arms with no log line were the two arms with the bug.** A
  silent early return in a state machine is a place the evidence
  cannot reach.
- **A healthy-looking link is not evidence of a healthy stream.** The
  heartbeat is deliberately outside the did-work gate, so
  `master_link_status:up` + `io 0s` says only that a timer fired.
  (`master_sync_in_progress` is a hardcoded `0` in INFO — it proved
  nothing in three sets of forensics.)
- **An explore map is a lead, not a fact.** The map for this arc said
  `promote_stop_runners` "does only `fetch_add`"; the source has an
  `if was_replica` gate, and the first reproduction attempt failed on
  exactly that. Read the decisive line yourself.
- **Contention was the wrong lever.** The recipe was a one-tick
  ordering window; the right move was to construct the ordering
  directly rather than to shake the machine until it happened.
