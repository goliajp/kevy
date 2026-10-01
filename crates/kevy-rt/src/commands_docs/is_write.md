Whether this command mutates the keyspace (so it must be logged to the AOF).

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
// RPUSH mutates, so it is logged to the AOF and replicated; PING is not.
assert!(Kv.is_write(&argv(&["RPUSH", "q", "x"])));
assert!(!Kv.is_write(&argv(&["PING"])));
```
