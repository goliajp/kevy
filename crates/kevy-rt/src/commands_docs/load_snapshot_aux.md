Take back the frame [`Commands::snapshot_aux`] kept beside a snapshot
this shard just loaded: at boot, before the log replays over it
(`full_sync = false`), or on a replica after a full sync from its primary
(`full_sync = true`), where it is the primary's state as of that
snapshot and `None` means the primary kept nothing beside it. The default
ignores it.

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
let frame = kevy_rt::Argv::from(vec![b"XINTERNAL.EXAMPLE".to_vec()]);
// the default keeps nothing, so it has nothing to take back
Minimal.load_snapshot_aux(Some(&frame), true);
assert!(Minimal.snapshot_aux().is_none());
```
