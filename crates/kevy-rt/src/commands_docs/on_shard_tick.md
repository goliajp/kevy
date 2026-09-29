Periodic shard housekeeping (the equivalent of Redis's `serverCron`).
kevy uses this to run [`Store::tick_expire`] at the configured
`[expiry].hz`. Default no-op so non-kevy embedders / runtimes can
ignore it.

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
// The default housekeeping does nothing to the store.
Minimal.on_shard_tick(&mut store);
assert_eq!(store.dbsize(), 0);
```
