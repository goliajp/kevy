`CLIENT KILL …` — all-shard fanout; each shard closes its
matching conns, the origin sums (or maps the legacy positional
form to `+OK` / `-ERR`).

```
use kevy_rt::Route;

let route = |sub: &str| if sub == "KILL" { Route::ClientKill } else { Route::Local };
assert_eq!(route("KILL"), Route::ClientKill);
```
