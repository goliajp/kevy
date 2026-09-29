Per-tick persistence-stats publication: whether this shard has a
background save/rewrite in flight and how many AOF rewrites have
completed since open. Command layers that serve `INFO persistence`
stash these in a thread-local (thread-per-core: the answering
thread *is* the shard, same pattern as [`Self::on_shard_start`]).
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

// What an `INFO persistence` implementation keeps per shard.
thread_local! { static REWRITES: Cell<u64> = const { Cell::new(0) }; }
let on_persist_stats = |_in_flight: bool, total: u64| REWRITES.with(|r| r.set(total));

on_persist_stats(false, 2);
assert_eq!(REWRITES.with(Cell::get), 2);

Minimal.on_persist_stats(true, 2);
```
