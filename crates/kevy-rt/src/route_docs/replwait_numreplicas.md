How many replicas the caller wants acked. Reported per shard;
the origin answers the minimum across them.

```
use kevy_rt::Route;

// `WAIT 3 0` asks for three replicas.
let route = Route::ReplWait { numreplicas: 3, timeout_ms: 0 };
assert!(matches!(route, Route::ReplWait { numreplicas: 3, .. }));
```
