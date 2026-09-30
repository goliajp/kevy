Called once per client command at dispatch entry (before routing /
fan-out, so a multi-key command counts once). kevy uses it for
`INFO stats: total_commands_processed`. Hot path — keep it to a single
thread-local bump. Default no-op so non-kevy embedders pay nothing.

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

// What kevy does here: one thread-local bump.
thread_local! { static COMMANDS: Cell<u64> = const { Cell::new(0) }; }
let on_command = || COMMANDS.with(|c| c.set(c.get() + 1));

on_command();
on_command();
assert_eq!(COMMANDS.with(Cell::get), 2);

Minimal.on_command();
```
