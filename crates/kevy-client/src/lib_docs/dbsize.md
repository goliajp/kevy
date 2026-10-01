```
let mut conn = kevy_client::Connection::connect("mem://")?;
assert_eq!(conn.dbsize()?, 0);
conn.mset(&[(b"a", b"1"), (b"b", b"2")])?;
assert_eq!(conn.dbsize()?, 2);
# Ok::<(), kevy_client::KevyError>(())
```
