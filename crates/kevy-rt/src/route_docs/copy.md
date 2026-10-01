`COPY src dst [REPLACE]` — two keys, so the same hazard the
rename and list-move routes exist for: left to the catch-all
`Single(1)` the copy lands in the SOURCE's shard, where no later
read of the destination will ever look. Same-shard pairs take
one atomic op; cross-shard pairs run Read → Put, and need no
rollback because the read does not remove anything.

Why it cannot ride `Single(1)`, in one assertion:

```
use kevy_persist::Routing;
use kevy_rt::{Route, shard_of_key};
// A pair of ordinary key names on an eight-shard server.
let (src, dst) = (b"ca".as_slice(), b"cb".as_slice());
assert_ne!(shard_of_key(src, 8, Routing::KevyHash), shard_of_key(dst, 8, Routing::KevyHash));
// `Single(1)` hashes args[1] — the SOURCE — and runs the whole
// command there, so the copy would land in a shard no later read
// of `dst` ever looks at, while the reply said it worked.
assert!(matches!(Route::Copy, Route::Copy));
```
