```
use std::time::Duration;
use kevy_client::{Connection, PubsubEvent, Subscriber};

// a named in-process bus; a kevy:// URL behaves the same over TCP
const URL: &str = "mem://subscriber-doc";
let mut sub = Subscriber::connect(URL)?;
sub.subscribe(&[b"orders"])?;
assert!(matches!(sub.recv()?, PubsubEvent::Subscribe { count: 1, .. }));
Connection::connect(URL)?.publish(b"orders", b"#42")?;
assert_eq!(sub.recv_message()?, (b"orders".to_vec(), b"#42".to_vec()));
sub.set_read_timeout(Some(Duration::from_millis(20)))?;
assert!(sub.recv().is_err(), "nothing more was published");
# Ok::<(), kevy_client::KevyError>(())
```
