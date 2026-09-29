One-shot boot-replay verdict for this shard: bytes dropped past
the last replayable AOF frame (quarantined + truncated by the
repair) and whether the stop was a corrupt frame. Fires once,
after the shard's startup replay, before the listener accepts.
Non-zero drops mean the shard recovered less than its file held —
command layers surface it via `INFO persistence` so operators can
alert on it. Default: no-op.

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
// An implementation alerts when the shard recovered less than its file held.
let needs_alert = |dropped_bytes: u64, corrupt: bool| dropped_bytes > 0 || corrupt;
assert!(!needs_alert(0, false));
assert!(needs_alert(512, true));

Minimal.on_replay_report(512, true);
```
