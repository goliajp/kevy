```
use kevy_client::Connection;

let mut conn = Connection::connect("mem://")?;
conn.set(b"k", b"v")?;
// the escape hatch to the full typed store API
let Connection::Embedded(store) = &conn else { unreachable!("mem:// is in-process") };
assert_eq!(store.strlen(b"k")?, 1);
# Ok::<(), kevy_client::KevyError>(())
```
