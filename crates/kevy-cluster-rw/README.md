# kevy-cluster-rw

Read/write-split cluster client for [kevy](https://crates.io/crates/kevy).

Extends `kevy-client`'s slot-routing `ClusterClient` with role awareness:
write commands hit each shard's primary, read commands round-robin across
that shard's replicas (falling back to the primary if no replica is
connected). A per-command `READCONSISTENT` flag forces a read to its
primary for callers that need fresh data.

**Status:** scaffolding. Topology discovery (`CLUSTER NODES` with role
extension), routing, and `READCONSISTENT` land in subsequent tasks of
the v3-1 feature branch.

See `.claude/plans/2026-06-18-v3-cluster-plan.md` in the kevy repo for the
full execution plan.
