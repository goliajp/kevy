The shard tick fired `excess_us` microseconds later than its
interval asked — the reactor's own stall gauge (a long-blocking
iteration delays the tick by exactly its overrun). Called at
tick cadence (10 Hz), so implementations may do real work.

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

// A stall gauge: keep the worst overrun seen.
static WORST_US: AtomicU64 = AtomicU64::new(0);
let on_tick_gap = |excess_us: u64| WORST_US.fetch_max(excess_us, Ordering::Relaxed);

on_tick_gap(250);
on_tick_gap(40);
assert_eq!(WORST_US.load(Ordering::Relaxed), 250);

Minimal.on_tick_gap(250);
```
