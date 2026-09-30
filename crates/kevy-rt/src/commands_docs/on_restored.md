Called once per shard after its startup restore (snapshot load and log
replay), before it serves. `record` writes a frame to this shard's log
and syncs it to disk, answering whether it did: `false` when the shard
keeps no log or the write failed. The frame also goes to this shard's
replication stream, so a replica that resumes the stream after this
start receives it as it would any other change. A command set uses it
to record, at a point where the write is known durable, state it found
at startup that the log does not hold yet. Default: no-op.

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
let mut written = Vec::new();
// the default records nothing
Minimal.on_restored(&mut |frame| {
    written.push(frame.clone());
    true
});
assert!(written.is_empty());
```
