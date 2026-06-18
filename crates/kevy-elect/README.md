# kevy-elect

Quorum-based primary failover for [kevy](https://crates.io/crates/kevy)
— the v3-cluster Phase 1.5 layer on top of the v1.18 manual `REPLICAOF`
primitive.

Detect a primary's death by quorum heartbeat, run an offset-ordered
election among the live replicas, promote the winner via
`REPLICAOF NO ONE`, and retarget the survivors at the new primary.
Driven by an operator-declared peer list (no gossip discovery — the
peer set is static for the lifetime of a cluster generation).

**Anti-scope (locked):** no Raft / no log replication consensus / no
cross-DC / no online resharding. The kevy storage layer remains the
single source of truth; this crate only chooses who writes to it.

Wire protocol: see [`docs/protocol.md`](docs/protocol.md).
