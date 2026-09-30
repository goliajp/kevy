Called once per accepted client connection. kevy uses it for
`INFO stats: total_connections_received`. Default no-op.

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

// `total_connections_received`, kept per shard.
thread_local! { static ACCEPTED: Cell<u64> = const { Cell::new(0) }; }
let on_connection = || ACCEPTED.with(|c| c.set(c.get() + 1));

on_connection();
assert_eq!(ACCEPTED.with(Cell::get), 1);

Minimal.on_connection();
```
