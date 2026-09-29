`PSUBSCRIBE pattern [pattern ...]` / `PUNSUBSCRIBE [pattern ...]` —
like Subscribe/Unsubscribe but the conn registers Redis-glob
patterns; `PUBLISH` to a matching channel delivers a `pmessage`
frame. Connection-level (modifies this conn + shared pattern
registry).

```
# use kevy_rt::{Route};
let route = |verb: &str| if verb == "PSUBSCRIBE" { Route::Psubscribe } else { Route::Single(1) };
assert_eq!(route("PSUBSCRIBE"), Route::Psubscribe);
```
