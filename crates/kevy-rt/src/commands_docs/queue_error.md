Validate a command being queued inside `MULTI`. Returns an error
reply (already RESP-encoded, e.g. `-ERR unknown command …`) when
the command cannot be queued — an unknown verb or an arity
mismatch — in which case the caller answers with it instead of
`+QUEUED` and marks the transaction dirty so `EXEC` aborts with
`-EXECABORT`. `None` means "queue it". Default `None` keeps
embedders that don't model a verb table permissive.

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
// The default queues anything, even a verb it does not know.
assert_eq!(Minimal.queue_error(&argv(&["NOSUCHVERB"])), None);
```
