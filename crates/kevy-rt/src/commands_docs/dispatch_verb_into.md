Execute a command whose verb [`Self::resolve`] already identified,
appending the reply for `proto` to `out`. The runtime calls this on the
shard that executes the command, with the [`VerbId`][crate::VerbId] the
resolve on the connection's shard returned, so an implementation that
assigns ids can go straight to the verb's handler instead of matching the
verb name again. Default: ignore the id and call [`Self::dispatch_into`]
(RESP2) or [`Self::dispatch_into_resp3`] (RESP3).

An override must answer exactly as `dispatch_into` / `dispatch_into_resp3`
would for the same argv: the id is a shortcut, never a different command.

```
# use kevy_rt::{Argv, ArgvView, Commands, RespVersion, Route, Store, TxnKind, VerbId};
# #[derive(Clone)]
# struct Kv;
# impl Commands for Kv {
#     fn route<A: ArgvView + ?Sized>(&self, a: &A) -> Route {
#         if a.len() > 1 { Route::Single(1) } else { Route::Local }
#     }
#     fn dispatch<A: ArgvView + ?Sized>(&self, _s: &mut Store, _a: &A) -> Vec<u8> {
#         b"+PONG\r\n".to_vec()
#     }
#     fn is_quit<A: ArgvView + ?Sized>(&self, _a: &A) -> bool {
#         false
#     }
#     fn is_write<A: ArgvView + ?Sized>(&self, _a: &A) -> bool {
#         false
#     }
#     fn txn_kind<A: ArgvView + ?Sized>(&self, _a: &A) -> TxnKind {
#         TxnKind::Other
#     }
# }
# fn argv(parts: &[&str]) -> Argv {
#     Argv::from(parts.iter().map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>())
# }
let mut store = Store::new();
let mut out = Vec::new();
// The default ignores the id and answers as dispatch_into does.
Kv.dispatch_verb_into(&mut store, &argv(&["PING"]), VerbId::UNKNOWN, RespVersion::V2, &mut out);
assert_eq!(out, b"+PONG\r\n");
```
