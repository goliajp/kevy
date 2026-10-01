```
use kevy_cluster_rw::ReadWriteClient;
use kevy_resp::Reply;
# mod doc { include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../kevy-resp-client/tests/doc_server/serve.rs")); }
# let ((primary, primary_key), (replica, replica_key)) = (doc::serve_secure(), doc::serve_secure());
# let (primary_key, replica_key) = (doc::hex(&primary_key), doc::hex(&replica_key));

let mut c = ReadWriteClient::connect_urls(
    &format!("kevys://127.0.0.1:{primary}?server_key={primary_key}"),
    &[&format!("kevys://127.0.0.1:{replica}?server_key={replica_key}")],
)?;
assert_eq!(c.replica_count(), 1);
let set = [b"SET".to_vec(), b"k".to_vec(), b"v".to_vec()];
assert_eq!(c.request(&set)?, Reply::Simple(b"OK".to_vec()));
# Ok::<(), std::io::Error>(())
```
