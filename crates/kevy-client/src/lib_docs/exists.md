```
let mut conn = kevy_client::Connection::connect("mem://")?;
conn.set(b"a", b"1")?;
assert_eq!(conn.exists(&[b"a", b"missing"])?, 1);
assert_eq!(conn.exists(&[b"a", b"a"])?, 2); // a repeated key counts each time
# Ok::<(), kevy_client::KevyError>(())
```
