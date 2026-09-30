Polled once per shard as it leaves the reactor loop: `true` when
the operator requested a final snapshot before exit (`SHUTDOWN
SAVE`). The shard then runs one background save and drains it
before the process exits. Default `false` — plain stops (SIGTERM,
bare SHUTDOWN) drain in-flight persistence but don't force a new
snapshot.

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
// A plain stop never forces a final snapshot.
assert!(!Minimal.shutdown_save_requested());
```
