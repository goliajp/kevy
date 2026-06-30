# kevy-replicate

Primary-to-replica streaming replication for kevy. Pure Rust, zero
`crates.io` dependencies.

A single primary streams every applied mutation to N read replicas
over a long-lived TCP connection, framed in a RESP-extended format
that carries a monotonic offset envelope. New replicas first receive
an inline snapshot, then catch up live from the frame stream.

Used by both the kevy server (the primary side and the
server-as-replica runner) and the embedded library
(`Store::open_replica`).

## Audience

Internal infrastructure for the kevy server and the
`kevy-embedded` library. End users configure replication via the
server's `[replication]` TOML section or `Store::open_replica` /
`Config::with_replica_upstream` on the embedded side. See
[`docs/replication.md`](https://github.com/goliajp/kevy/blob/develop/docs/replication.md).

## License

MIT OR Apache-2.0, at your option.
