The keyspace event announced when a command's result is placed on its
destination's shard ([`Route::StoreFromCopies`](crate::Route::StoreFromCopies)).
Default: the verb, lower-cased — `ZRANGESTORE` announces `zrangestore`.
A command whose event is named otherwise (Redis's `SORT … STORE`
announces `sortstore`) says so here.

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
assert_eq!(Minimal.placed_event(&argv(&["ZRANGESTORE", "d", "s", "0", "-1"])), b"zrangestore");
```
