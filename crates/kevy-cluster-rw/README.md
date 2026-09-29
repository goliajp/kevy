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
use kevy_cluster_rw::{ReadConsistency, ReadWriteClient};

let mut c = ReadWriteClient::connect(
    ("primary.internal", 6004),
    &[("replica-a.internal", 6004), ("replica-b.internal", 6004)],
)?;

c.request(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?; // → primary
let v = c.request(&[b"GET".to_vec(), b"k".to_vec()])?;         // → some replica (round-robin)
let fresh = c.request_read(&[b"GET".to_vec(), b"k".to_vec()], ReadConsistency::Primary)?;
# Ok::<(), std::io::Error>(())
```

Replica selection is round-robin across the configured replica list.
`ReadConsistency::Primary` forces a read to the primary
(`READCONSISTENT` semantics) for callers that need fresh data.
`ReadWriteClient::connect_urls` takes `kevy://` / `kevys://` URLs for
encrypted client ports.

## Audience

Application-facing wrapper for kevy primary-replica deployments. See
[`docs/replication.md`](https://github.com/goliajp/kevy/blob/develop/docs/replication.md)
for the server-side configuration.

## License

MIT OR Apache-2.0, at your option.
