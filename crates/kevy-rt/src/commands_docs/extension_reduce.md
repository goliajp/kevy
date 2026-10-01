Origin-side reduce of an extension fan-out — merge every
shard's chunk (produced by [`Self::extension_op`]) into either
the final RESP reply or a follow-up fan-out argv (see
[`ExtensionReduced`]). `proto` is the requesting connection's
negotiated RESP version so proto-aware reduces can shape the
reply (Map vs pair-array).

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
use kevy_rt::{ExtensionReduced, RespVersion};

let argv = vec![b"IDX.SEARCH".to_vec()];
let chunks = vec![Vec::new(), Vec::new()];
// Without an extension the reduce answers the client with an error.
assert!(matches!(
    Minimal.extension_reduce(&argv, chunks, RespVersion::V2),
    ExtensionReduced::Reply(e) if e.starts_with(b"-ERR")
));
```
