```
use kevy_client::{Connection, KevyError};

// two opens of one named URL share one store
let mut a = Connection::connect("mem://connect-doc")?;
let mut b = Connection::connect("mem://connect-doc")?;
a.set(b"k", b"v")?;
assert_eq!(b.get(b"k")?, Some(b"v".to_vec()));
// kevy has no TLS, so rediss:// is refused before any connect
assert!(matches!(Connection::connect("rediss://localhost"), Err(KevyError::Unsupported(_))));
# Ok::<(), kevy_client::KevyError>(())
```
