//! v1.27.3: HMSET dispatch. Deprecated `HMSET` alias of HSET that
//! still ships in BullMQ scripts — verify it stores the pairs and
//! replies `+OK\r\n` (vs HSET's integer added-count).

use kevy_resp::Argv;
use kevy_store::Store;

fn argv(parts: &[&[u8]]) -> Argv {
    let mut a = Argv::default();
    for p in parts {
        a.push(p);
    }
    a
}

#[test]
fn hmset_returns_ok_and_stores_pairs() {
    let mut store = Store::new();
    let reply = kevy::dispatch(
        &mut store,
        &argv(&[b"HMSET", b"h", b"f1", b"v1", b"f2", b"v2"]),
    );
    assert_eq!(reply, b"+OK\r\n");
    let g1 = kevy::dispatch(&mut store, &argv(&[b"HGET", b"h", b"f1"]));
    assert_eq!(g1, b"$2\r\nv1\r\n");
    let g2 = kevy::dispatch(&mut store, &argv(&[b"HGET", b"h", b"f2"]));
    assert_eq!(g2, b"$2\r\nv2\r\n");
}

#[test]
fn hmset_overwrites_existing_field() {
    let mut store = Store::new();
    kevy::dispatch(&mut store, &argv(&[b"HSET", b"h", b"f1", b"old"]));
    let reply = kevy::dispatch(&mut store, &argv(&[b"HMSET", b"h", b"f1", b"new"]));
    assert_eq!(reply, b"+OK\r\n");
    let g1 = kevy::dispatch(&mut store, &argv(&[b"HGET", b"h", b"f1"]));
    assert_eq!(g1, b"$3\r\nnew\r\n");
}

#[test]
fn hmset_wrong_arity_errors() {
    // odd-count: HMSET key f1 v1 f2 (missing value for f2).
    let mut store = Store::new();
    let reply = kevy::dispatch(&mut store, &argv(&[b"HMSET", b"h", b"f1", b"v1", b"f2"]));
    assert!(
        reply.starts_with(b"-ERR wrong number of arguments"),
        "got {:?}",
        String::from_utf8_lossy(&reply)
    );
}

#[test]
fn hmset_on_wrong_type_errors() {
    let mut store = Store::new();
    kevy::dispatch(&mut store, &argv(&[b"SET", b"s", b"str"]));
    let reply = kevy::dispatch(&mut store, &argv(&[b"HMSET", b"s", b"f", b"v"]));
    assert!(
        reply.starts_with(b"-WRONGTYPE "),
        "got {:?}",
        String::from_utf8_lossy(&reply)
    );
}
