Keyspace-wide invalidation hook: called after FLUSHALL/FLUSHDB
has emptied this shard's store (both the client path and the
replica apply path execute the same op). Synchronous index
maintenance resets its derived structures here — a flushed
keyspace must not keep answering from stale index entries.

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
store.flushall();
// Runs after the flush: the store is already empty.
Minimal.on_flush(&mut store);
assert_eq!(store.dbsize(), 0);
# Ok::<(), Box<dyn std::error::Error>>(())
```
