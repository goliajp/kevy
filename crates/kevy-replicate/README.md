# kevy-replicate

Primary-to-replica streaming replication for [kevy](https://crates.io/crates/kevy).

Phase 1 of the v3-cluster series: a single primary streams every applied
mutation to N read replicas over a long-lived TCP connection, using a
RESP3-extended frame format carrying a monotonic offset envelope. New
replicas first receive an inline snapshot, then catch up live from the
frame stream.

**Status:** scaffolding. Wire format, source, replica, and snapshot
modules land in subsequent tasks of the v3-1 feature branch.

See `.claude/plans/2026-06-18-v3-cluster-plan.md` in the kevy repo for the
full execution plan.
