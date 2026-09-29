Execute a command, appending the RESP reply to `out`. The in-order local
fast path uses this to write straight into the connection's output buffer
(no per-command reply `Vec`). Default: delegate to [`dispatch`](Self::dispatch).

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
let ping = argv(&["PING"]);
// Replies append, so a pipelined batch lands in one output buffer.
let mut out = Vec::new();
Kv.dispatch_into(&mut store, &ping, &mut out);
Kv.dispatch_into(&mut store, &ping, &mut out);
assert_eq!(out, b"+PONG\r\n+PONG\r\n");
```
