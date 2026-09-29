`BITOP op dst src [src …]` — N sources gathered, combined, and
stored at a destination that sits at `args[2]`, not `args[1]`.
`ZAlgebraStore` is the same shape with a different payload: it
combines set and zset members, not raw bytes.

Carries nothing. An earlier draft carried the operator so the
router could pick it, which meant parsing the operator twice and
needing a fallback route for the argv the router could not parse
— and that fallback led to a dispatch table with no BITOP arm,
so a malformed BITOP would have been answered "unknown command".
The route says only that this is a BITOP; every refusal is
worded once, in `exec_bitop`.

Why it cannot ride `Single(1)`, in one assertion:

```
use kevy_persist::Routing;
use kevy_rt::{Route, shard_of_key};
// `Single(1)` hashes args[1]. For BITOP that is the OPERATOR.
let operator = b"AND".as_slice();
let destination = b"dst".as_slice();
assert_ne!(shard_of_key(operator, 8, Routing::KevyHash), shard_of_key(destination, 8, Routing::KevyHash));
assert!(matches!(Route::BitOpStore, Route::BitOpStore));
```
