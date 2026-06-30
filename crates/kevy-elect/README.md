# kevy-elect

Quorum-based primary failover for kevy. Pure Rust, zero `crates.io`
dependencies.

Detects a primary's death by quorum heartbeat, runs an offset-ordered
election among the live replicas, promotes the winner via
`REPLICAOF NO ONE`, and retargets the survivors at the new primary.
Driven by an operator-declared peer list — there is no gossip
discovery; the peer set is static for the lifetime of a cluster
generation.

## Out of scope

- No Raft. No log-replication consensus.
- No cross-DC failover.
- No online resharding.

The kevy storage layer remains the single source of truth; this
crate only chooses who writes to it.

## Audience

Internal infrastructure activated by the kevy server when the
operator configures `[cluster] peers` and `[cluster] node_id`. End
users configure failover via the server's TOML — see
[`docs/replication.md`](https://github.com/goliajp/kevy/blob/develop/docs/replication.md)
and [`docs/cluster.md`](https://github.com/goliajp/kevy/blob/develop/docs/cluster.md).

## License

MIT OR Apache-2.0, at your option.
