```
let mut conn = kevy_client::Connection::connect("mem://")?;
conn.mset(&[(b"a", b"1"), (b"b", b"2")])?;
conn.flushall()?;
assert_eq!(conn.dbsize()?, 0);
# Ok::<(), kevy_client::KevyError>(())
```
