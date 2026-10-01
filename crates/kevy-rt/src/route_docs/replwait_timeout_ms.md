Deadline in milliseconds. `0` is Redis's wait-forever form and
is hard-capped by the runtime rather than honoured literally.

```
use kevy_rt::Route;

// `WAIT 1 0` is the wait-forever form.
let route = Route::ReplWait { numreplicas: 1, timeout_ms: 0 };
assert!(matches!(route, Route::ReplWait { timeout_ms: 0, .. }));
```
