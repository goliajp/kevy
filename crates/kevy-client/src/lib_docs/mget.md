```
let mut conn = kevy_client::Connection::connect("mem://")?;
conn.set(b"a", b"1")?;
conn.set(b"c", b"3")?;
let got = conn.mget(&[b"a", b"b", b"c"])?;
assert_eq!(got, vec![Some(b"1".to_vec()), None, Some(b"3".to_vec())]);
# Ok::<(), kevy_client::KevyError>(())
```
