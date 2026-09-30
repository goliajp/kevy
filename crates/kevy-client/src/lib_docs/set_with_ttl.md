```
use std::time::Duration;

let mut conn = kevy_client::Connection::connect("mem://")?;
conn.set_with_ttl(b"otp", b"123456", Duration::from_millis(50))?;
assert_eq!(conn.get(b"otp")?, Some(b"123456".to_vec()));
std::thread::sleep(Duration::from_millis(100));
assert_eq!(conn.get(b"otp")?, None); // expired
# Ok::<(), kevy_client::KevyError>(())
```
