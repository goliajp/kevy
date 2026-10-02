[`Commands::block_restore_argv`] with the serve replay in hand, for a
kind whose undo depends on more than the key — how many a `BZMPOP …
COUNT n` takes, and from which end. The runtime calls this one; the
default answers what `block_restore_argv` answers, so an implementation
that only overrides that keeps working.

```
# use kevy_rt::{Argv, ArgvView, Commands, Route, Store, TxnKind};
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
use kevy_rt::BlockKind;

let mut store = Store::new();
store.rpush(b"q", &[b"x".as_slice()])?;
let serve = Argv::from(vec![b"BLMPOP".to_vec(), b"0".to_vec(), b"1".to_vec(), b"q".to_vec(), b"LEFT".to_vec()]);
// The default defers to `block_restore_argv`, which has nothing to put back.
assert_eq!(Minimal.block_restore_for(&mut store, BlockKind::Blmpop, &serve, b"q"), None);
# Ok::<(), Box<dyn std::error::Error>>(())
```
