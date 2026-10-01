```
# use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
# let port = kevy_testnet::free_port();
# let dir = std::env::temp_dir().join(format!("kevy-client-doc-{}-{port}", std::process::id()));
# std::fs::create_dir_all(&dir)?;
# let stop = Arc::new(AtomicBool::new(false));
# let server = std::thread::spawn({
#     let (stop, dir) = (stop.clone(), dir.clone());
#     move || {
#         kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
#             .bind([127, 0, 0, 1], port)
#             .shards(1)
#             .with_data_dir(dir)
#             .run(stop)
#     }
# });
# kevy_testnet::assert_listening(port, "kevy");
use kevy_client::Connection;

let mut conn = Connection::connect(&format!("kevy://127.0.0.1:{port}"))?;
let mut txn = conn.multi()?;
txn.set(b"n", b"1")?.incr(b"n")?.get(b"n")?;
let mut replies = txn.exec_typed()?;
assert_eq!(replies.remaining(), 3);
replies.next_ok()?; // SET
assert_eq!(replies.next_int()?, 2); // INCR
assert_eq!(replies.next_bulk()?, Some(b"2".to_vec())); // GET
replies.expect_empty()?;
# stop.store(true, Ordering::SeqCst);
# let _ = std::net::TcpStream::connect(("127.0.0.1", port));
# server.join().unwrap()?;
# std::fs::remove_dir_all(&dir)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```
