```
let mut conn = kevy_client::Connection::connect("mem://")?;
conn.set(b"a", b"1")?;
conn.set(b"b", b"2")?;
// "missing" does not count
assert_eq!(conn.del(&[b"a", b"b", b"missing"])?, 2);
assert_eq!(conn.get(b"a")?, None);
# Ok::<(), kevy_client::KevyError>(())
```
