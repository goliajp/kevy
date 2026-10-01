Per-tick AOF on-disk format gauge (the embedder ask's
server twin): 0 = AOF off, 1 = a pre-4.0 v1 file still being
appended (a 3.x binary swap-back still works), 2 = v2. Follows
[`Self::on_persist_stats`]'s shard-gauge pattern.

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
// How an implementation reads the gauge.
let describe = |format: u8| match format {
    0 => "off",
    1 => "v1",
    _ => "v2",
};
assert_eq!(describe(0), "off");
assert_eq!(describe(2), "v2");

Minimal.on_aof_format(2);
```
