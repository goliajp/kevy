The end of the destination to push onto: the head for
`RPOPLPUSH` and `LMOVE ... LEFT`, else the tail.

```
use kevy_rt::Route;
use kevy_store::ListEnd;

// `LMOVE src dst LEFT RIGHT` pushes onto the destination's tail.
let route = Route::ListMove { from: ListEnd::Left, to: ListEnd::Right };
assert!(matches!(route, Route::ListMove { to: ListEnd::Right, .. }));
```
