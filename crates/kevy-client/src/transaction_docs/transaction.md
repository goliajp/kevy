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
use kevy_client::{Connection, Reply};

let mut conn = Connection::connect(&format!("kevy://127.0.0.1:{port}"))?;
let mut txn = conn.multi()?;
txn.set(b"balance", b"100")?.incr_by(b"balance", -30)?;
// both commands run as one unit at EXEC
assert_eq!(txn.exec()?, [Reply::Simple(b"OK".to_vec()), Reply::Int(70)]);
// a transaction dropped without exec is discarded
conn.multi()?.set(b"balance", b"0")?;
assert_eq!(conn.get(b"balance")?, Some(b"70".to_vec()));
# stop.store(true, Ordering::SeqCst);
# let _ = std::net::TcpStream::connect(("127.0.0.1", port));
# server.join().unwrap()?;
# std::fs::remove_dir_all(&dir)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```
