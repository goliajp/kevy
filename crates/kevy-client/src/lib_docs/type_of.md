```
let mut conn = kevy_client::Connection::connect("mem://")?;
conn.set(b"s", b"v")?;
conn.hset(b"h", &[(b"field", b"v")])?;
assert_eq!(conn.type_of(b"s")?, "string");
assert_eq!(conn.type_of(b"h")?, "hash");
assert_eq!(conn.type_of(b"missing")?, "none");
# Ok::<(), kevy_client::KevyError>(())
```
