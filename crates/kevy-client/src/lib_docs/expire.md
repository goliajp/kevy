```
use std::time::Duration;

let mut conn = kevy_client::Connection::connect("mem://")?;
conn.set(b"session", b"x")?;
assert!(conn.expire(b"session", Duration::from_secs(60))?);
assert!(conn.ttl_ms(b"session")? > 59_000);
assert!(!conn.expire(b"missing", Duration::from_secs(60))?);
# Ok::<(), kevy_client::KevyError>(())
```
