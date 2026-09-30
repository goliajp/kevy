```
use std::time::Duration;

let mut conn = kevy_client::Connection::connect("mem://")?;
conn.set_with_ttl(b"k", b"v", Duration::from_secs(60))?;
assert!(conn.persist(b"k")?);
assert_eq!(conn.ttl_ms(b"k")?, -1); // no TTL any more
assert!(!conn.persist(b"k")?); // nothing left to remove
# Ok::<(), kevy_client::KevyError>(())
```
