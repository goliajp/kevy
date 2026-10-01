Key the result is written to — its shard takes the write, which
is why both keys have to be extracted before routing.

```
use kevy_rt::Route;

// `GEOSEARCHSTORE near cities ...`: `near` receives the result.
let route = Route::GeoStore { src: b"cities".to_vec(), dst: b"near".to_vec() };
assert!(matches!(route, Route::GeoStore { ref dst, .. } if dst == b"near"));
```
