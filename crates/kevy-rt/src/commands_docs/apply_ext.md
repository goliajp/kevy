Apply one message another shard's hook queued for this one (see
[`Commands::take_ext_out`]). Default: no-op.

```
use kevy_rt::{ArgvView, Commands, Route, Store, TxnKind};

#[derive(Clone)]
struct Minimal;
impl Commands for Minimal {
    fn route<A: ArgvView + ?Sized>(&self, _a: &A) -> Route { Route::Local }
    fn dispatch<A: ArgvView + ?Sized>(&self, _s: &mut Store, _a: &A) -> Vec<u8> {
        b"+OK\r\n".to_vec()
    }
    fn is_quit<A: ArgvView + ?Sized>(&self, _a: &A) -> bool { false }
    fn is_write<A: ArgvView + ?Sized>(&self, _a: &A) -> bool { false }
    fn txn_kind<A: ArgvView + ?Sized>(&self, _a: &A) -> TxnKind { TxnKind::Other }
}

let mut store = Store::new();
Minimal.apply_ext(&mut store, b"opaque to the runtime");
assert_eq!(store.dbsize(), 0);
```
