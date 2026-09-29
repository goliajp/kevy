`COUNT`, applied per stream rather than across the gather.

```
use kevy_rt::Route;

let streams = vec![(b"a".to_vec(), b"0".to_vec()), (b"b".to_vec(), b"0".to_vec())];
// `COUNT 10` caps each stream at ten entries, twenty in total here.
let route = Route::XReadGather { streams, count: Some(10), group: None };
assert!(matches!(route, Route::XReadGather { count: Some(10), .. }));
```
