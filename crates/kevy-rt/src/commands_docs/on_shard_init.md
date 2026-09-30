Called once per shard, immediately after [`Store::new`], before the
reactor enters its event loop. Implementations install per-shard
configuration that the runtime doesn't know about — currently the
`maxmemory` + eviction-policy pair, which kevy ships via its own
process-wide config snapshot. Default: no-op so non-kevy embedders
aren't forced to override.

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
// The default installs nothing; the fresh store is left as it was.
Minimal.on_shard_init(&mut store);
assert_eq!(store.dbsize(), 0);
```
