# kevy-cluster-rw

A read/write-split client wrapper for kevy. Routes writes to the
primary and round-robins reads across the replicas of a
primary-replica topology.

## Install

```sh
cargo add kevy-cluster-rw
```

## Example

```rust,no_run
use kevy_cluster_rw::ClusterRwClient;

let mut c = ClusterRwClient::connect(
    "primary.internal:6379",
    &["replica-a.internal:6379", "replica-b.internal:6379"],
)?;

c.set(b"k", b"v")?;             // → primary
let v = c.get(b"k")?;           // → some replica (round-robin)
# Ok::<(), std::io::Error>(())
```

Replica selection is round-robin across the configured replica list.
Replica health is tracked per-connection; a failed replica falls out
of rotation until the next reconnect succeeds. A per-command
`READCONSISTENT` flag forces a read to the primary for callers that
need fresh data.

## Audience

Application-facing wrapper for kevy primary-replica deployments. See
[`docs/replication.md`](https://github.com/goliajp/kevy/blob/develop/docs/replication.md)
for the server-side configuration.

## License

MIT OR Apache-2.0, at your option.
