Pre-dispatch write gate. `Some(err_bytes)` rejects every
data-write client command with that RESP error before any
routing (replication apply does NOT pass through here, so a
read-only replica keeps applying its feed). Admin verbs
(REPLICAOF / CONFIG) are not classified as writes and stay
available as the operator escape hatch. Default: writes always
allowed.

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
// Writes are allowed unless an implementation says otherwise.
assert_eq!(Minimal.write_denied(), None);
```
