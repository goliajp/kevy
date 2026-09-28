//! An embedded store reads every index under its own locks, so a global
//! partitioning has nothing to save it: refused by name, while `local`,
//! which every embedded index is, is accepted.

#![cfg(feature = "index")]

use kevy_embedded::{Config, Store};

fn create(s: &Store, tail: &[&[u8]]) -> String {
    let argv: Vec<Vec<u8>> =
        [b"IDX.CREATE".as_slice(), b"age", b"ON", b"PREFIX", b"u:", b"FIELD", b"age"]
            .iter()
            .chain([b"TYPE".as_slice(), b"i64", b"KIND", b"range"].iter())
            .chain(tail.iter())
            .map(|a| a.to_vec())
            .collect();
    let mut out = Vec::new();
    s.dispatch_argv(&argv, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn a_global_partitioning_is_refused_by_name_and_local_is_accepted() {
    let s = Store::open(Config::default()).expect("open");
    let refused = create(&s, &[b"PARTITION", b"global", b"SPLIT", b"30"]);
    assert!(refused.contains("PARTITION global is a server feature"), "{refused}");
    assert_eq!(create(&s, &[b"PARTITION", b"local"]), "+OK\r\n");
}
