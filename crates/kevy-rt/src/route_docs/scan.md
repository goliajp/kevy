`SCAN cursor [MATCH pattern] [COUNT count] [TYPE type]` — a real
cursor iterator: each call visits ~COUNT buckets of ONE shard
(chaining into the next shard only while the work budget lasts)
and replies `[next-cursor, keys]`. `Err` carries why the command
layer refused the arguments (invalid cursor / syntax error); the
runtime replies its wire form.

The cursor is the raw wire cursor: the runtime splits it into
`(shard, in-shard position)` — shard index in the top 10 bits,
reverse-binary bucket cursor in the low 54. Cursors are therefore
only meaningful on the server (and shard count) that issued them,
like Redis Cluster cursors are per-node.

```
use kevy_rt::{Argv, Route};

// `SCAN 0 COUNT 5`, parsed at routing time.
let argv = Argv::from(vec![b"SCAN".to_vec(), b"0".to_vec(), b"COUNT".to_vec(), b"5".to_vec()]);
let route = Route::Scan(kevy_verbs::args::scan_opts(&argv));
assert!(matches!(route, Route::Scan(Ok(ref o)) if o.count == 5));
```
