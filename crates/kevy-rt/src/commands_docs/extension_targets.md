The shards an extension read needs; `None` sends it to every shard, as
before this hook existed. An index spread over the shards by value names
only the shards holding the value range asked for. Default: `None`.

The runtime asks again for every follow-up phase
([`ExtensionReduced::Continue`](crate::ExtensionReduced::Continue)) with
that phase's argv, so a read that walks the shards in order names one
shard per phase. A list names each shard once and at least one shard: the
reply waits for a chunk from every shard named.

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
