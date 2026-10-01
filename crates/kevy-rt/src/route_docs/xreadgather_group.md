`Some` turns each per-shard sub-query into an XREADGROUP, which
makes it a write: the PEL update happens on the stream's own
shard and is logged there.

```
use kevy_rt::{Route, XGroupCtx};

// `XREADGROUP GROUP g c STREAMS a b > >`
let streams = vec![(b"a".to_vec(), b">".to_vec()), (b"b".to_vec(), b">".to_vec())];
let group = Some(XGroupCtx::new(b"g".to_vec(), b"c".to_vec()));
let route = Route::XReadGather { streams, count: None, group };
assert!(matches!(route, Route::XReadGather { group: Some(_), .. }));
```
