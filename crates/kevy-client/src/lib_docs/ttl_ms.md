```
use std::time::Duration;

let mut conn = kevy_client::Connection::connect("mem://")?;
assert_eq!(conn.ttl_ms(b"k")?, -2); // no such key
conn.set(b"k", b"v")?;
assert_eq!(conn.ttl_ms(b"k")?, -1); // no TTL
conn.expire(b"k", Duration::from_secs(10))?;
assert!((9_000..=10_000).contains(&conn.ttl_ms(b"k")?));
# Ok::<(), kevy_client::KevyError>(())
```
