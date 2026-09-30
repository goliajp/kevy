Per-tick live-connection gauge: how many client conns this
shard currently holds (cluster-bus links excluded). Command
layers publish it to their cross-shard stats slots so `INFO`
`connected_clients` sums a real instance-wide value. Default:
no-op.

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
use core::sync::atomic::{AtomicU64, Ordering};

// A gauge: each tick replaces the reading.
static LIVE: AtomicU64 = AtomicU64::new(0);
let on_conn_gauge = |live: u64| LIVE.store(live, Ordering::Relaxed);

on_conn_gauge(12);
on_conn_gauge(9);
assert_eq!(LIVE.load(Ordering::Relaxed), 9);

Minimal.on_conn_gauge(9);
```
