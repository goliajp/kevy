Called after every applied write with the written key
(when the resolver knew one). Default no-op; kevy uses it for
synchronous secondary-index maintenance (derived-by-
construction). Runs on the shard thread with store access —
implementations must be cheap when their feature is off.

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
let mut store = Store::new();
store.rpush(b"q", &[b"x".as_slice()])?;
// Called after the write landed; the default maintains nothing.
Minimal.on_write(&mut store, b"q");
assert_eq!(store.llen(b"q")?, 1);
# Ok::<(), Box<dyn std::error::Error>>(())
```
