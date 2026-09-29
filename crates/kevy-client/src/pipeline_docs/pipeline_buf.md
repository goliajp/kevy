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
use kevy_client::{Connection, KevyError, Reply};

let mut conn = Connection::connect(&format!("kevy://127.0.0.1:{port}"))?;
let replies = conn.pipeline(|p| {
    p.cmd(&[b"SET", b"n", b"1"]).cmd(&[b"INCR", b"n"]);
    assert_eq!(p.len(), 2); // queued client-side, nothing sent yet
})?;
assert_eq!(replies, [Reply::Simple(b"OK".to_vec()), Reply::Int(2)]);
// the in-process backend has nothing to pipeline to
let mut mem = Connection::connect("mem://")?;
assert!(matches!(mem.pipeline(|p| { p.cmd(&[b"PING"]); }), Err(KevyError::Unsupported(_))));
# stop.store(true, Ordering::SeqCst);
# let _ = std::net::TcpStream::connect(("127.0.0.1", port));
# server.join().unwrap()?;
# std::fs::remove_dir_all(&dir)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```
