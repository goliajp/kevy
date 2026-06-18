# kevy-elect protocol (v1.19.0 / v3-cluster Phase 1.5)

Quorum-based primary failover. Detect a primary's death by majority
heartbeat, elect a successor by **highest offset → lowest node-id**,
and retarget the survivors at the new primary.

This is a **control plane** — `kevy-elect` does not move keyspace
data. The actual catch-up from the new primary still flows through
`kevy-replicate`'s existing wire (backlog + snapshot ship). All
`kevy-elect` does is decide who writes.

## Anti-scope (locked, do not file issues for these)

- **Not Raft.** No log replication consensus. The kevy keyspace is
  the single source of truth; we only elect who writes to it.
- **No gossip discovery.** Peer set is operator-declared and static
  for the cluster generation. Add/remove a peer = rolling restart.
- **No cross-DC.** Heartbeat RTT assumptions are LAN-scale.
- **No on-line topology change.** Resharding / membership change is
  a rolling restart, never online.

## Wire format

All messages are RESP2 multi-bulk arrays — same parser as the
keyspace plane, so `kevy-elect` reuses `kevy-resp`'s
`parse_command_borrowed` for decode.

Transport is **TCP** (one persistent connection per `(peer_a,
peer_b)` ordered pair). UDP was considered and rejected: kevy-elect
messages are rare (~1 / `hb_interval_ms`) and the simplicity win of
single-flight ordered delivery + no packet-loss retry logic
outweighs UDP's lower latency on a LAN. The control connection is
distinct from the data-plane replication TCP listener — different
ports, different fds, different parsers.

Listener port: `cluster.elect_port_base + shard_id`, mirroring the
cluster + replication listener pattern (Issue Ledger I2 stays
applicable). Single-shard kevy node = 1 elect port. Multi-shard
node = N elect ports — but **election state is per-node, not
per-shard** (a node's "I am primary" / "I am replica" flag applies
to all its shards in lock-step). Shard 0's elect port is the
canonical listener; other shards' ports exist only for symmetry
with the listener layout, and accept connections only for parity
with future per-shard quorum (Phase 2+).

## Node identity

Every kevy node has a stable **node id** — an opaque string from the
operator config. Conventions for the operator:

- Recommended shape: `kebab-case` (`primary-east`, `replica-1`).
- MUST be unique across the cluster.
- ≤ 32 bytes, ASCII-only (escape rules would otherwise bleed into
  every protocol frame).

The node id is **separate from the replication `replica_id`** (which
is per-shard per-process, ephemeral). Election uses node id;
replication wire uses `replica_id`. They are not the same string.

## Epoch

Every election attempt is stamped with a monotonic `epoch: u64`.
Initial epoch = `1`. A candidate increments the epoch when it
broadcasts `OFFER-LEADER`. Peers reject any election message
referencing an epoch ≤ the highest epoch they have seen, so a
partitioned-then-healed node can't replay stale `ACCEPT` votes
against a fresher election.

After a successful election, the epoch is broadcast in `ANNOUNCE`
and stamped on every subsequent heartbeat by the new primary. A
stale node receiving an `HB` with epoch > its own demotes immediately
and retargets replication.

## Message types

### `HB <epoch> <node_id> <role> <repl_offset>` — heartbeat

Sent every `hb_interval_ms` (default `200`) by **every** node to
**every** other peer. Carries:

- `epoch` (u64) — the election epoch this node believes is current.
- `node_id` (bulk) — sender's node id.
- `role` (`primary` | `replica` | `candidate`) — sender's current
  self-perceived role.
- `repl_offset` (u64) — sender's highest applied replication offset.
  Primary reports its own `source.next_offset()`; replica reports
  its `expected_offset` after applying the latest frame.

Heartbeat is unsolicited and stateless on the receiver — it updates
the per-peer last-seen timestamp + cached `(epoch, role, offset)`
view. There is no `HB-ACK`; the next outbound `HB` from the receiver
serves the same purpose.

### `OFFER <new_epoch> <candidate_id> <repl_offset>` — election trigger

Broadcast by a replica that has flagged the primary DOWN AND has
won the candidate-selection rule (highest offset → lowest node-id)
among its currently-seen peers.

A peer receiving `OFFER` decides:

- If `new_epoch <= last_seen_epoch` → reject silently (stale).
- If sender's `repl_offset` < my own → reject silently (a better
  candidate exists).
- Else → record vote-cast for this epoch, reply `ACCEPT
  <new_epoch> <my_node_id>`.

Each peer can cast **at most one ACCEPT per epoch** (the prevents
two candidates from each gathering a quorum in the same epoch). A
peer that has already accepted this epoch does NOT need to remember
which candidate it accepted — the candidate accumulates its own
ACCEPTs, and only a candidate with a quorum can ANNOUNCE.

### `ACCEPT <epoch> <accepter_id>` — vote

Replied by a peer to the candidate's `OFFER`. The candidate counts
ACCEPTs per epoch; on reaching `N/2 + 1` (including its own implicit
self-vote), it transitions to primary and broadcasts `ANNOUNCE`.

### `ANNOUNCE <epoch> <new_primary_id> <new_primary_addr>` — commit

Broadcast by the newly-elected primary. Every peer receiving this
with `epoch > current_epoch`:

1. Updates `current_epoch` and `current_primary`.
2. If self-role was `primary` and `new_primary_id != self.node_id`
   → demote (`REPLICAOF NO ONE`-equivalent in-process state + drop
   downstream `replication_listener`).
3. Retargets `kevy-replicate` ReplicaRunner to
   `new_primary_addr` (REPLICAOF host port equivalent in-process).
4. Drops any in-flight ACCEPT vote for this epoch (commit point).

## State machine

```
       initial (role = config-declared)
              │
              ▼
       ┌──────────────┐  hb timeout +    ┌──────────────┐
       │   replica    ├─ I win election ─▶  candidate   │
       │  (current    │  per offset rule │ (sent OFFER, │
       │   primary    │                  │  collecting  │
       │   alive)     ◀── ANNOUNCE for ──┤   ACCEPT)    │
       └──────┬───────┘   newer epoch    └──────┬───────┘
              │                                 │
              │ ANNOUNCE for newer epoch        │ got quorum ACCEPT
              │ where new_primary != self       │   in time
              │                                 ▼
       ┌──────▼───────┐                  ┌──────────────┐
       │   replica    │  ANNOUNCE        │   primary    │
       │ (retargeted) ◀──── for newer ───┤              │
       │              │      epoch       │              │
       └──────────────┘                  └──────────────┘
```

Transitions:

- `replica → candidate`: DOWN detector flagged primary AND I win
  candidate-selection rule among live peers.
- `candidate → primary`: collected `N/2 + 1` ACCEPTs for my epoch
  within `election_timeout_ms`.
- `candidate → replica`: ANNOUNCE for newer epoch arrived (lost the
  race to another candidate), OR `election_timeout_ms` expired (back
  off `election_backoff_ms`, then re-arm DOWN detector).
- `primary → replica`: ANNOUNCE for a newer epoch where I am not
  the new primary (split-brain healing).
- `replica → replica`: ANNOUNCE for newer epoch where new_primary
  is some peer — retarget replication.

## DOWN detection

Per-peer `last_seen_ts: Instant`. A peer is flagged DOWN by me when:

```
now - last_seen_ts >= down_after_ms
```

I do NOT declare DOWN globally on my own — I publish my DOWN view
in my own `HB`'s implicit content (peers I haven't heard from in
`down_after_ms` are absent from my `seen` list). Actually for
simplicity, DOWN is a local decision per node; quorum DOWN emerges
when `N/2 + 1` replicas simultaneously decide to vote for a new
candidate. The candidate-selection step is what gates it: a single
node that mistakenly thinks the primary is down still needs `N/2 +
1` ACCEPTs to ANNOUNCE, which it won't get if other peers still see
HBs from the primary.

`down_after_ms` default: `5_000`. `hb_interval_ms` default: `200`.
Ratio 25× means a transient ≤ 1 s blip doesn't trigger an election.

## Timeouts (defaults)

| param | default | meaning |
|---|---|---|
| `hb_interval_ms` | 200 | period between outbound `HB` per peer |
| `down_after_ms` | 5_000 | mark a peer DOWN after this many ms without `HB` |
| `election_timeout_ms` | 3_000 | candidate waits this long for quorum ACCEPT before backoff |
| `election_backoff_ms` | 1_000–5_000 (random) | candidate re-arms DOWN detector after this jitter |

## Tie-breaking

When two replicas have the same `repl_offset` and both detect
primary DOWN at the same instant, both may start candidacy in the
same epoch. The deterministic tiebreak is **lowest node-id wins**
(byte-lexicographic comparison). The losing candidate sees the
winner's `OFFER` with the same `repl_offset` and a lower
`candidate_id`, rejects its own self-promotion, and casts ACCEPT
for the winner.

## Quorum + split-brain

Quorum = `N/2 + 1` where N = peer count from config. Examples:

- N=3 → 2 ACCEPTs needed → tolerates 1 failure.
- N=5 → 3 ACCEPTs needed → tolerates 2 failures.
- N=2 → 2 ACCEPTs needed → **no fault tolerance**. The config linter
  warns "N=2 has no fault tolerance". Either node going down locks
  the cluster (intentional — better than split-brain).

Partition: minority partition cannot reach quorum → stays read-only
(no writes via `REPLICAOF NO ONE` semantics; existing primary on
the majority side keeps writing). On partition heal, the minority's
replicas catch up via the regular replication backlog / snapshot
ship path. If the minority's primary was the *old* primary, it sees
ANNOUNCE for a newer epoch on rejoin and demotes cleanly.

## Edge cases handled

- **Old primary rejoin**: ANNOUNCE for epoch > self.epoch →
  demote + retarget. If self has writes that the new primary
  doesn't (split-brain writes during partition), they are **lost**
  on full-sync. This is the cost of "highest offset wins" — the
  minority side never claimed quorum, so its writes never had
  guaranteed durability.
- **Dueling candidates with same offset**: lower node-id wins by
  the deterministic tiebreak.
- **Network partition + heal at exactly the wrong moment**: epoch
  monotonicity ensures only one election can succeed per epoch; a
  rejoining ex-primary sees a higher epoch and demotes.
- **2-node degenerate**: quorum=2 means either down locks. Config
  linter warns at startup; operator's choice.

## What this protocol does NOT solve

- **Data freshness across partition**: minority writes that never
  reached the majority are lost on rejoin. Use `READCONSISTENT` on
  the read side to avoid stale reads, but the write side cannot
  retroactively repair split-brain writes.
- **Network instability flapping**: aggressive `down_after_ms`
  causes elections during transient blips. Tune to your RTT;
  defaults assume a single LAN.
- **Clock skew**: HB timestamps are receiver-local (`Instant`), not
  cross-host wall-clock. Skew between hosts doesn't matter.
