```
use kevy_cluster_rw::{ReadConsistency, ReadWriteClient};
use kevy_resp::Reply;
# mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/doc_server/node.rs")); }
# let nodes = [(); 3].map(|_| doc::node(kevy_testnet::free_port()));
# let [primary, replica_a, replica_b] = [0, 1, 2].map(|i| nodes[i].port);

let mut c = ReadWriteClient::connect(
    ("127.0.0.1", primary),
    &[("127.0.0.1", replica_a), ("127.0.0.1", replica_b)],
)?;
c.request(&[b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()])?; // primary
// a replica, round-robin; the nodes here do not replicate, so it has no `k`
assert_eq!(c.request(&[b"GET".to_vec(), b"k".to_vec()])?, Reply::Nil);
let fresh = c.request_read(&[b"GET".to_vec(), b"k".to_vec()], ReadConsistency::Primary)?;
assert_eq!(fresh, Reply::Bulk(b"v".to_vec()));
# Ok::<(), std::io::Error>(())
```
