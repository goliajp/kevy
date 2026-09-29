```
use kevy_cluster_rw::{ReadConsistency, ReadWriteClient};
use kevy_resp::Reply;
# mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/node.rs")); }
# let (p, r) = (doc::node(kevy_testnet::free_port()), doc::node(kevy_testnet::free_port()));
# let (primary, replica) = (p.port, r.port);

let mut c = ReadWriteClient::connect(("127.0.0.1", primary), &[("127.0.0.1", replica)])?;
c.request_write(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?;
let reply = c.request_read(&[b"GET".to_vec(), b"k".to_vec()], ReadConsistency::Primary)?;
assert_eq!(reply, Reply::Bulk(b"v".to_vec()));
# Ok::<(), std::io::Error>(())
```
