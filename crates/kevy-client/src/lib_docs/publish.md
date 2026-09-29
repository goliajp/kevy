```
use kevy_client::{Connection, PubsubEvent, Subscriber};

const URL: &str = "mem://publish-doc";
let mut sub = Subscriber::connect_channels(URL, &[b"news"])?;
assert!(matches!(sub.recv()?, PubsubEvent::Subscribe { .. })); // the ack
let mut conn = Connection::connect(URL)?;
assert_eq!(conn.publish(b"news", b"hello")?, 1); // one receiver
assert_eq!(sub.recv_message()?, (b"news".to_vec(), b"hello".to_vec()));
assert_eq!(conn.publish(b"sports", b"score")?, 0); // nobody listens there
# Ok::<(), kevy_client::KevyError>(())
```
