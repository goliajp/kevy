Per-tick replication-view publication: the answering shard's
current `master_repl_offset` (== `ReplicationSource::next_offset()`)
plus a [`ReplicaViewRow`] for every handshake-complete replica
conn (in `AckSent`, `Streaming`, or `SnapshotShipping`); the
row's `ack` is `None` until the replica's first `REPLCONF ACK`.
Only called when this shard has a `ReplicationSource`
installed (i.e. `Runtime::with_replication(true)` was
requested); standalone setups pay nothing. Command layers
that serve `ROLE` / `INFO replication` stash the values in a
thread-local (thread-per-core: the answering thread *is* the
shard, same pattern as [`Self::on_persist_stats`]) and may
additionally publish them to a shared slot for cross-shard
aggregation. Default no-op.

```
# use kevy_rt::{ArgvView, Commands, Route, Store, TxnKind};
# #[derive(Clone)]
# struct Minimal;
# impl Commands for Minimal {
#     fn route<A: ArgvView + ?Sized>(&self, _a: &A) -> Route { Route::Local }
#     fn dispatch<A: ArgvView + ?Sized>(&self, _s: &mut Store, _a: &A) -> Vec<u8> {
#         b"+OK\r\n".to_vec()
#     }
#     fn is_quit<A: ArgvView + ?Sized>(&self, _a: &A) -> bool { false }
#     fn is_write<A: ArgvView + ?Sized>(&self, _a: &A) -> bool { false }
#     fn txn_kind<A: ArgvView + ?Sized>(&self, _a: &A) -> TxnKind { TxnKind::Other }
# }
use kevy_rt::{ReplicaAck, ReplicaViewRow};
use std::net::Ipv4Addr;

let rows: Vec<ReplicaViewRow> = vec![
    ("replica-a".to_string(), Ipv4Addr::LOCALHOST, 7001, 120, Some(ReplicaAck::new(100, 5))),
    ("replica-b".to_string(), Ipv4Addr::LOCALHOST, 7002, 120, None),
];
// What `INFO replication` derives: each replica's lag behind the primary.
let master_repl_offset = 120;
let lag: Vec<Option<u64>> =
    rows.iter().map(|r| r.4.map(|ack| master_repl_offset - ack.acked_offset)).collect();
assert_eq!(lag, [Some(20), None]);

Minimal.on_replication_view(master_repl_offset, rows);
```
