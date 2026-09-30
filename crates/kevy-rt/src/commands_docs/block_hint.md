Classify a command for blocking semantics. `BlockHint::None`
(default) is the zero-cost answer for every non-blocking verb;
the dispatcher only registers a waiter when this returns
`BlockHint::Block` *and* the command's `dispatch_into` produced no
reply (i.e. it could not satisfy itself immediately — e.g. BLPOP
on an empty list). Concrete impls should fold this into their
override of [`Self::resolve`] so the verb-table lookup happens
once per command.

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
use kevy_rt::BlockHint;

// Without an override no command blocks, BLPOP included.
assert_eq!(Minimal.block_hint(&argv(&["BLPOP", "q", "0"])), BlockHint::None);
```
