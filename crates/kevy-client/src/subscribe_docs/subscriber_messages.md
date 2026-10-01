```
use kevy_client::{Connection, Subscriber};

const URL: &str = "mem://subscriber-messages-doc";
let mut sub = Subscriber::connect_channels(URL, &[b"news"])?;
let mut conn = Connection::connect(URL)?;
conn.publish(b"news", b"one")?;
conn.publish(b"news", b"two")?;
// the subscribe ack is skipped; only payloads come out
let got: Vec<(Vec<u8>, Vec<u8>)> = sub.messages().take(2).collect::<Result<_, _>>()?;
assert_eq!(got[0], (b"news".to_vec(), b"one".to_vec()));
assert_eq!(got[1].1, b"two");
# Ok::<(), kevy_client::KevyError>(())
```
