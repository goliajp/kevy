The end of the source to pop from: the head for
`LMOVE ... LEFT ...`, the tail for `RPOPLPUSH`.

```
use kevy_rt::Route;
use kevy_store::ListEnd;

// `LMOVE src dst LEFT RIGHT` pops the source's head.
let route = Route::ListMove { from: ListEnd::Left, to: ListEnd::Right };
assert!(matches!(route, Route::ListMove { from: ListEnd::Left, .. }));
```
