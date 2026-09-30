RESP3 variant of [`Self::dispatch_into`] — called when the
connection has negotiated `HELLO 3`. Default: delegate to the
RESP2 path (so a server that hasn't migrated any replies still
works correctly with a RESP3 client, per spec). Override per
command to emit RESP3 shapes (Map / Set / Double / …).

```
# use kevy_rt::{Argv, ArgvView, Commands, Route, Store, TxnKind};
# #[derive(Clone)]
# struct Kv;
# impl Commands for Kv {
#     fn route<A: ArgvView + ?Sized>(&self, a: &A) -> Route {
#         if a.len() > 1 { Route::Single(1) } else { Route::Local }
#     }
#     fn dispatch<A: ArgvView + ?Sized>(&self, s: &mut Store, a: &A) -> Vec<u8> {
#         match a.first() {
#             Some(b"RPUSH") => match s.rpush(&a[1], &[&a[2]]) {
#                 Ok(n) => format!(":{n}\r\n").into_bytes(),
#                 Err(_) => b"-WRONGTYPE\r\n".to_vec(),
#             },
#             _ => b"+PONG\r\n".to_vec(),
#         }
#     }
#     fn is_quit<A: ArgvView + ?Sized>(&self, a: &A) -> bool {
#         a.first() == Some(&b"QUIT"[..])
#     }
#     fn is_write<A: ArgvView + ?Sized>(&self, a: &A) -> bool {
#         a.first() == Some(&b"RPUSH"[..])
#     }
#     fn txn_kind<A: ArgvView + ?Sized>(&self, a: &A) -> TxnKind {
#         match a.first() {
#             Some(b"MULTI") => TxnKind::Multi,
#             Some(b"EXEC") => TxnKind::Exec,
#             _ => TxnKind::Other,
#         }
#     }
# }
# fn argv(parts: &[&str]) -> Argv {
#     Argv::from(parts.iter().map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>())
# }
let mut store = Store::new();
// Without an override a RESP3 connection gets the RESP2 reply.
let mut out = Vec::new();
Kv.dispatch_into_resp3(&mut store, &argv(&["PING"]), &mut out);
assert_eq!(out, b"+PONG\r\n");
```
