```
use kevy_client::{Connection, PubsubEvent, Subscriber};

const URL: &str = "mem://subscriber-events-doc";
let mut sub = Subscriber::connect_channels(URL, &[b"news"])?;
Connection::connect(URL)?.publish(b"news", b"hello")?;
let mut events = sub.events();
// acks come through as well as messages
assert!(matches!(events.next().transpose()?, Some(PubsubEvent::Subscribe { .. })));
let msg = PubsubEvent::Message { channel: b"news".to_vec(), payload: b"hello".to_vec() };
assert_eq!(events.next().transpose()?, Some(msg));
# Ok::<(), kevy_client::KevyError>(())
```
