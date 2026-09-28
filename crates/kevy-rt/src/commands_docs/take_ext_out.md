Messages the write hooks queued for other shards since the last take,
as `(target shard, payload)`. A client's write that queued any holds its
reply until each has been applied where it was sent, so a read issued after
the reply sees what the write changed there. Default: none.

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

assert!(Minimal.take_ext_out().is_empty());
```
