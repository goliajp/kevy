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

#[test]
fn idx_list_names_every_index_local_in_the_servers_shape() {
    let s = Store::open(Config::default()).expect("open");
    assert_eq!(create(&s, &[]), "+OK\r\n");
    s.hset(b"u:1", &[(b"age".as_slice(), b"30".as_slice())]).expect("hset");
    let mut out = Vec::new();
    s.dispatch_argv(&[b"IDX.LIST".to_vec()], &mut out);
    let list = String::from_utf8_lossy(&out);
    // one row of ten pairs, the last the partitioning a server also names
    assert!(list.starts_with("*1\r\n*20\r\n$4\r\nname\r\n$3\r\nage\r\n"), "{list}");
    assert!(list.contains("$7\r\nentries\r\n$1\r\n1\r\n"), "{list}");
    assert!(list.ends_with("$12\r\npartitioning\r\n$5\r\nlocal\r\n"), "{list}");
}
