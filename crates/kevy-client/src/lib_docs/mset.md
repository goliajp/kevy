```
let mut conn = kevy_client::Connection::connect("mem://")?;
conn.mset(&[(b"a", b"1"), (b"b", b"2")])?;
assert_eq!(conn.mget(&[b"a", b"b"])?, vec![Some(b"1".to_vec()), Some(b"2".to_vec())]);
# Ok::<(), kevy_client::KevyError>(())
```
