Single-key; route by `args[idx]`.

```
use kevy_rt::{Argv, Route};

// `GET k`: the key sits at index 1, so that is what gets hashed.
let get = Argv::from(vec![b"GET".to_vec(), b"k".to_vec()]);
if let Route::Single(i) = Route::Single(1) {
    assert_eq!(&get[i], b"k");
}
```
