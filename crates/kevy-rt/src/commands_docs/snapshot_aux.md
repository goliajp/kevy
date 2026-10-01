State the command set keeps outside the store, as one record frame the
runtime stores beside this shard's keyspace: in every snapshot (a save,
a replica's full sync) and in every rewritten log, so the state survives
whatever replaces the log it was first recorded in. `None` (the default)
= nothing to keep. [`Commands::load_snapshot_aux`] takes it back.

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
// a command set with nothing outside the store keeps nothing beside it
assert!(Minimal.snapshot_aux().is_none());
```
