Non-blocking `XREAD` / `XREADGROUP` over **multiple** streams — fan
each stream out to its owning shard and merge the per-stream replies
in request order (single-stream forms still route via
[`Self::Single`]). Each element is `(stream key, last-seen id)`;
`count` is the optional `COUNT` cap applied per stream; `group`
`Some` makes each per-shard sub-query an `XREADGROUP` (a write —
PEL / last-delivered updates happen on each stream's owning shard
and are AOF-logged there as the rewritten single-stream command).
The command set builds this only for the non-blocking, ≥2-stream
forms; blocking reads park on the origin shard instead (see the
cross-shard BLOCK arbiter).

```
use kevy_rt::Route;

// `XREAD COUNT 2 STREAMS a b 0 0`
let streams = vec![(b"a".to_vec(), b"0".to_vec()), (b"b".to_vec(), b"0".to_vec())];
let route = Route::XReadGather { streams, count: Some(2), group: None };
assert!(matches!(route, Route::XReadGather { group: None, .. }));
```
