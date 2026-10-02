Whether a result placed on its destination's shard
(`Route::StoreFromCopies`) keeps the
deadline the destination already had. Default `false`: a command that
replaces its destination (`ZRANGESTORE`, `SORT … STORE`) clears it, while
one that updates it in place (`PFMERGE`) keeps it.

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
# fn argv(parts: &[&str]) -> kevy_rt::Argv {
#     kevy_rt::Argv::from(parts.iter().map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>())
# }
assert!(!Minimal.placed_keeps_ttl(&argv(&["ZRANGESTORE", "d", "s", "0", "-1"])));
```
