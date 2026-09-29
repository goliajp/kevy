Handle `HELLO` — return the new connection protocol version + the
reply bytes. The runtime applies the new version to the conn
before scheduling the reply, so a `HELLO 3` ack itself comes out
shaped as a RESP3 Map (the new protocol is in effect for its own
reply).

Default: ignore the args, keep `current_proto`, emit a minimal
RESP2 +OK so embedders that don't care still see a sane reply.
kevy's own impl in `kevy::KevyCommands` parses the optional
protover and emits the full server-info shape.

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
use kevy_rt::RespVersion;

// The default keeps the protocol and answers +OK, even to `HELLO 3`.
let (proto, reply) = Minimal.hello_reply(&argv(&["HELLO", "3"]), RespVersion::V2);
assert_eq!(proto, RespVersion::V2);
assert_eq!(reply, b"+OK\r\n");
```
