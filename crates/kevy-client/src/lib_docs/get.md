```
let mut conn = kevy_client::Connection::connect("mem://")?;
assert_eq!(conn.get(b"k")?, None);
conn.set(b"k", b"v")?;
assert_eq!(conn.get(b"k")?, Some(b"v".to_vec()));
# Ok::<(), kevy_client::KevyError>(())
```
