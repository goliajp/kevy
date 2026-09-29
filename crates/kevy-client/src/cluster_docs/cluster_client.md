```
# use std::io::Write;
# use std::net::TcpListener;
# // a stand-in node: serves each accepted connection from one script of replies
# fn node(l: TcpListener, scripts: Vec<Vec<Vec<u8>>>) {
#     std::thread::spawn(move || {
#         for script in scripts {
#             let (mut s, _) = l.accept().unwrap();
#             let mut pending = Vec::new();
#             for reply in script {
#                 if !kevy_testnet::read_request(&mut s, &mut pending) {
#                     break;
#                 }
#                 s.write_all(&reply).unwrap();
#             }
#         }
#     });
# }
# let (a, b) = (TcpListener::bind("127.0.0.1:0")?, TcpListener::bind("127.0.0.1:0")?);
# let (pa, pb) = (a.local_addr()?.port(), b.local_addr()?.port());
# let slots = format!(
#     "*2\r\n*3\r\n:0\r\n:8191\r\n*2\r\n$9\r\n127.0.0.1\r\n:{pa}\r\n\
#      *3\r\n:8192\r\n:16383\r\n*2\r\n$9\r\n127.0.0.1\r\n:{pb}\r\n"
# );
# // node a is the seed (answers CLUSTER SLOTS), then serves its own slots
# node(a, vec![vec![slots.into_bytes()], vec![b"$6\r\nfrom-a\r\n".to_vec()]]);
# node(b, vec![vec![b"$6\r\nfrom-b\r\n".to_vec()]]);
use kevy_client::ClusterClient;

// two nodes: a owns slots 0..=8191, b owns 8192..=16383
let mut c = ClusterClient::connect("127.0.0.1", pa)?;
assert_eq!(c.shard_count(), 2);
// "a" hashes to slot 15495, so the GET goes straight to node b
assert_eq!(kevy_hash::key_hash_slot(b"a"), 15495);
assert_eq!(c.get(b"a")?, Some(b"from-b".to_vec()));
assert_eq!(kevy_hash::key_hash_slot(b"b"), 3300);
assert_eq!(c.get(b"b")?, Some(b"from-a".to_vec()));
# Ok::<(), Box<dyn std::error::Error>>(())
```
