Search half of a geo `*STORE` (`GEOSEARCHSTORE` / `GEORADIUS…STORE`),
run on the SOURCE key's shard: match `argv`'s query against the source
zset and return the `(member, score)` pairs to write — the scores
already in their final form (geohash, or the STOREDIST distance in the
unit the command asked for). The runtime writes them at the
destination's own shard. A command set
that doesn't route [`Route::GeoStore`] never sees this call.

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
use kevy_rt::GeoHits;

let mut store = Store::new();
let argv = vec![b"GEOSEARCHSTORE".to_vec(), b"dst".to_vec(), b"src".to_vec()];
// A command set without geo answers the search half with an error reply.
assert!(matches!(
    Minimal.geo_search(&mut store, &argv),
    GeoHits::Error(e) if e.starts_with(b"-ERR")
));
```
