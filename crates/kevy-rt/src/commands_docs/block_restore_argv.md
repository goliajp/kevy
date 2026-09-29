The command that would put back whatever replaying `serve_argv` is
about to consume — read from the store **before** the serve runs.

A cross-shard serve pops on the target and ships the reply to the
origin. If the origin's client disconnected in that window the
reply has nowhere to go, and the element would be lost: taken
from the list, delivered to nobody. The origin cannot put it back
(it holds a RESP frame whose shape differs per kind *and* per
negotiated protocol), so the target captures the undo first and
holds it until the origin confirms delivery.

Read, not parse: the peek runs on the owning shard immediately
before the pop with nothing interleaved, so what it saw is what
the pop takes, in RESP2 and RESP3 alike.

`None` = nothing to undo. That is the honest answer for kinds
that consume nothing (`XREAD` is non-destructive) and the safe
default for an embedder that has not implemented it.

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
use kevy_rt::BlockKind;

let mut store = Store::new();
store.rpush(b"q", &[b"x".as_slice()])?;
// The default has nothing to put back.
assert_eq!(Minimal.block_restore_argv(&mut store, BlockKind::Blpop, b"q"), None);
# Ok::<(), Box<dyn std::error::Error>>(())
```
