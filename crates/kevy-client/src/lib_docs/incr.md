```
let mut conn = kevy_client::Connection::connect("mem://")?;
assert_eq!(conn.incr(b"n")?, 1); // a missing key starts from 0
assert_eq!(conn.incr(b"n")?, 2);
conn.set(b"word", b"abc")?;
assert!(conn.incr(b"word").is_err());
# Ok::<(), kevy_client::KevyError>(())
```
