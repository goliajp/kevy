The reply to send if any shard misses its deadline, pre-built by
the command layer because it names the upstream primary — the
runtime does not know that address.

```
use kevy_rt::Route;

let miss = b"-MISDIRECTED writer is 10.0.0.1:6004\r\n".to_vec();
let route = Route::ReplBarrier { offsets: vec![0], timeout_ms: 250, miss };
assert!(matches!(route, Route::ReplBarrier { ref miss, .. } if miss.starts_with(b"-MISDIRECTED")));
```
