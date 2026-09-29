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
conn.hset(b"item:a", &[(&b"qty"[..], &b"9"[..])])?;
conn.hset(b"item:b", &[(&b"qty"[..], &b"10"[..])])?;
conn.idx_create_range(b"idx", b"item:", b"qty", IdxType::I64)?;
let page = ready(|| conn.idx_query_range(b"idx", b"2", b"10", 10, None))?;
let keys: Vec<&[u8]> = page.rows.iter().map(|r| r.key.as_slice()).collect();
assert_eq!(keys, [&b"item:a"[..], &b"item:b"[..]]);
# stop.store(true, Ordering::SeqCst);
# let _ = std::net::TcpStream::connect(("127.0.0.1", port));
# server.join().unwrap()?;
# std::fs::remove_dir_all(&dir)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```
