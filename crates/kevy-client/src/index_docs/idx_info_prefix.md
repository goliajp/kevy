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
# fn ready<T>(mut f: impl FnMut() -> kevy_client::KevyResult<T>) -> kevy_client::KevyResult<T> {
#     loop {
#         match f() {
#             Err(e) if e.to_string().contains("INDEXBUILDING") => {
#                 std::thread::sleep(std::time::Duration::from_millis(20))
#             }
#             r => return r,
#         }
#     }
# }
use kevy_client::{Connection, IdxType};

let mut conn = Connection::connect(&format!("kevy://127.0.0.1:{port}"))?;
for (user, age) in [("user:1", "31"), ("user:2", "25"), ("user:3", "42")] {
    conn.hset(user.as_bytes(), &[(&b"age"[..], age.as_bytes())])?;
}
conn.idx_create_range(b"by_age", b"user:", b"age", IdxType::I64)?;
// only keys under this prefix are indexed
assert_eq!(conn.idx_list()?[0].prefix, b"user:");
# stop.store(true, Ordering::SeqCst);
# let _ = std::net::TcpStream::connect(("127.0.0.1", port));
# server.join().unwrap()?;
# std::fs::remove_dir_all(&dir)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```
