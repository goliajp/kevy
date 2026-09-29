Called once on the shard's own thread, first thing in the reactor
entry (both reactors), before restore/replay. Implementations that
need per-shard identity at dispatch time (e.g. kevy's `CLUSTER MYID`
/ `CLUSTER NODES` `myself` flag) stash `shard` in a thread-local here
— in a thread-per-core runtime the current thread *is* the shard.
Default: no-op.

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
use std::cell::Cell;

// What an implementation does with the argument: remember which shard
// this thread is, for dispatch-time answers such as `CLUSTER MYID`.
thread_local! { static SHARD: Cell<Option<usize>> = const { Cell::new(None) }; }
let on_shard_start = |shard: usize| SHARD.with(|s| s.set(Some(shard)));

on_shard_start(3);
assert_eq!(SHARD.with(Cell::get), Some(3));

// The default does nothing.
Minimal.on_shard_start(3);
```
