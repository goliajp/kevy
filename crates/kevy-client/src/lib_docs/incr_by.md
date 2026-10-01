```
let mut conn = kevy_client::Connection::connect("mem://")?;
assert_eq!(conn.incr_by(b"stock", 10)?, 10);
assert_eq!(conn.incr_by(b"stock", -3)?, 7); // a negative delta decrements
# Ok::<(), kevy_client::KevyError>(())
```
