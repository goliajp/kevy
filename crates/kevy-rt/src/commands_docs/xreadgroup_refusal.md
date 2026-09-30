The check half of an `XREADGROUP` whose streams live on different
shards, run on each stream's own shard before any of them is read:
`argv` is that stream's single-stream `XREADGROUP`, and the answer is
the error reply the command would give it (unknown key or group, a
stream of another type, a bad ID), or `None` when it would read. When
any part is refused the runtime answers the first refusal in request
order and reads nothing, so a refused command leaves every group as
it was. A command set that answers `None` everywhere reads each part
as it comes.

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
let argv = kevy_rt::Argv::from(vec![b"XREADGROUP".to_vec(), b"GROUP".to_vec()]);
assert_eq!(Minimal.xreadgroup_refusal(&mut store, &argv), None);
```
