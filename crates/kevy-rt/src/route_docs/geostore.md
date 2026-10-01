Geo `*STORE` family — `GEOSEARCHSTORE dst src …` and
`GEORADIUS[BYMEMBER] src … STORE|STOREDIST dst`.

These MUST be routed, not left to the catch-all `Route::Single(1)`:
GEOSEARCHSTORE puts the DESTINATION at `argv[1]` (so the search then
read the source off the wrong shard — `:0`, or "could not decode
requested zset member" for FROMMEMBER) while GEORADIUS puts the
SOURCE there (so the destination was written into the source's
shard, invisible to every later read of it). Both keys are carried
here because neither sits at a fixed argv index — the legacy forms
hide `dst` behind an option-soup scan.

The search runs on `src`'s shard ([`crate::Commands::geo_search`]),
the write lands on `dst`'s (`Op::ZStoreResult`) — see
the runtime's geo-store orchestration.

```
use kevy_rt::Route;

// `GEOSEARCHSTORE dst src ...`: the destination comes first on the wire.
let route = Route::GeoStore { src: b"src".to_vec(), dst: b"dst".to_vec() };
assert!(matches!(route, Route::GeoStore { ref src, .. } if src == b"src"));
```
