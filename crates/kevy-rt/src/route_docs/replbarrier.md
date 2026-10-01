`REPL.WAIT` on a replica — all-shard applied barrier:
shard `i` answers once its replication-apply position reaches
`offsets[i]` (or the deadline passes). All met → `+OK`; any
timeout → the pre-built `miss` reply (kevy sends
`-MISDIRECTED writer is <primary>`). The command layer builds
`miss` because the upstream address is its knowledge, not the
runtime's.

```
use kevy_rt::Route;

// `REPL.WAIT` with a token from a two-shard primary.
let route = Route::ReplBarrier { offsets: vec![10, 7], timeout_ms: 100, miss: b"-MISDIRECTED\r\n".to_vec() };
assert!(matches!(route, Route::ReplBarrier { ref offsets, .. } if offsets.len() == 2));
```
