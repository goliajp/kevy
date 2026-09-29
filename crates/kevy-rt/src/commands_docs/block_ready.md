Non-destructive readiness peek for a parked waiter: would replaying
`serve_argv` (built by [`Self::block_serve_argv`], `$` already
frozen) produce a reply right now? Runs on the key's owning shard
when arming and is the gate for emitting a cross-shard wake. Must
NOT mutate the store (no pop / no group-cursor advance). Default
`false` so non-blocking embedders never spuriously wake.

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
# fn argv(parts: &[&str]) -> kevy_rt::Argv {
#     kevy_rt::Argv::from(parts.iter().map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>())
# }
use kevy_rt::BlockKind;

let mut store = Store::new();
store.rpush(b"q", &[b"x".as_slice()])?;
// The default never reports ready, so it never wakes a waiter.
assert!(!Minimal.block_ready(&mut store, &argv(&["BLPOP", "q", "0"]), BlockKind::Blpop));
// And it did not touch the list.
assert_eq!(store.llen(b"q")?, 1);
# Ok::<(), Box<dyn std::error::Error>>(())
```
