The shards an extension read needs; `None` sends it to every shard, as
before this hook existed. An index spread over the shards by value names
only the shards holding the value range asked for. Default: `None`.

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

let argv = vec![b"IDX.QUERY".to_vec()];
assert_eq!(Minimal.extension_targets(&argv), None);
```
