Per-shard half of an extension fan-out command (IDX.* /
future VIEW.* / FT.*): compute this shard's raw chunk for
`argv`. The payload encoding is the embedder's own — the
runtime treats it as opaque bytes and hands all chunks to
[`Commands::extension_reduce`] at the origin.

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
let mut store = Store::new();
// Without an extension, every shard contributes an empty chunk.
let argv = vec![b"IDX.SEARCH".to_vec(), b"books".to_vec()];
assert!(Minimal.extension_op(&mut store, &argv).is_empty());
```
