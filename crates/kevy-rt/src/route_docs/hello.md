`HELLO [protover [AUTH user pass] [SETNAME name]]` — server
handshake; on `HELLO 3` flips the conn into RESP3 mode (per-conn
`proto` field). Reply shape itself is proto-aware (V2: array of
pairs; V3: Map). Connection-level, dispatch via the
[`crate::Commands::hello_reply`] hook so embedders set their own server
metadata.

```
# use kevy_rt::{Route};
let route = |verb: &str| if verb == "HELLO" { Route::Hello } else { Route::Single(1) };
assert_eq!(route("HELLO"), Route::Hello);
```
