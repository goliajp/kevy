```
let mut conn = kevy_client::Connection::connect("mem://")?;
conn.set(b"greeting", b"hello")?;
conn.set(b"greeting", b"hi")?; // unconditional: overwrites
assert_eq!(conn.get(b"greeting")?, Some(b"hi".to_vec()));
# Ok::<(), kevy_client::KevyError>(())
```
