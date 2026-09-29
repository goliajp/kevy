`(stream key, start id)` per stream, already paired — the wire
form lists all keys and then all ids, which is not routable.

```
use kevy_rt::Route;

// `STREAMS a b 1-0 5-0` pairs up as (a, 1-0) and (b, 5-0).
let streams = vec![(b"a".to_vec(), b"1-0".to_vec()), (b"b".to_vec(), b"5-0".to_vec())];
let route = Route::XReadGather { streams, count: None, group: None };
assert!(matches!(route, Route::XReadGather { ref streams, .. } if streams[1].1 == b"5-0"));
```
