`WAIT numreplicas timeout` — all-shard barrier: each
shard answers (possibly deferred until its replicas ACK or the
deadline) with how many of its replicas acked its
`master_repl_offset` at arm time; the origin replies the MIN.
`timeout_ms == 0` = the Redis "wait forever" form (the runtime
hard-caps it — see `exec_replwait::WAIT_HARD_CAP_MS`).

```
use kevy_rt::Route;

// `WAIT 2 500`
let route = Route::ReplWait { numreplicas: 2, timeout_ms: 500 };
assert!(matches!(route, Route::ReplWait { numreplicas: 2, .. }));
```
