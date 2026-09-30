Snapshot of the runtime-owned knobs that can be hot-modified
(the kevy server wires this to `CONFIG SET`). Called once per
shard tick — each `Some` value is applied to the shard's live
state; each `None` keeps the existing setting untouched.

Default returns all-None so embedders that never hot-swap config
pay nothing beyond one struct-build per tick. The cost lives in
the impl's read of its own config source.

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
use kevy_rt::LiveRuntimeConfig;

// The default snapshot changes nothing on the shard.
let live = Minimal.live_runtime_config();
assert_eq!(live, LiveRuntimeConfig::default());
assert!(live.tick_interval_ms.is_none());
```
