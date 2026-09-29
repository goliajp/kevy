`RPOPLPUSH src dst` / `LMOVE src dst LEFT|RIGHT LEFT|RIGHT` /
`BRPOPLPUSH src dst timeout`, once the blocking form has an element
to serve.

These MUST be routed, not left to `Route::Single(1)`. The source and
the destination are different keys and can live on different shards;
the catch-all route hashes `args[1]` (the source), so the destination
push executed on the SOURCE's shard and the element was written into
a keyspace nobody would ever read it from. It returned the moved
value, so the caller believed it had worked. Measured on an 8-shard
server: 11 of 12 moves silently lost the element.

Same-shard pairs are one atomic Op on the owning shard. Cross-shard
pairs run the Take→Push orchestrator (mirroring [`Self::Rename`]),
which is NOT atomic — see `exec_listmove`.

```
use kevy_rt::Route;
use kevy_store::ListEnd;

// `RPOPLPUSH src dst`: pop the tail, push the head.
let route = Route::ListMove { from: ListEnd::Right, to: ListEnd::Left };
assert!(matches!(route, Route::ListMove { .. }));
```
