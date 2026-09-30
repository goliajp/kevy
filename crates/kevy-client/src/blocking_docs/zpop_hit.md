```
use std::time::Duration;
use kevy_client::{Connection, ZPopHit};

let mut conn = Connection::connect("mem://")?;
conn.zadd(b"jobs", &[(2.0, &b"later"[..]), (1.0, &b"soon"[..])])?;
let hit: Option<ZPopHit> = conn.bzpopmin(&[b"jobs"], Some(Duration::from_millis(100)))?;
// (key, member, score): the lowest score comes out first
assert_eq!(hit, Some((b"jobs".to_vec(), b"soon".to_vec(), 1.0)));
# Ok::<(), kevy_client::KevyError>(())
```
