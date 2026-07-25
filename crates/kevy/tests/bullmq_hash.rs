//! HMSET dispatch. Deprecated `HMSET` alias of HSET that
//! still ships in BullMQ scripts — verify it stores the pairs and
//! replies `+OK\r\n` (vs HSET's integer added-count).

use kevy_resp::Argv;
use kevy_store::Store;

/// In-process dispatcher: one KevyCommands per test thread, so
/// per-state caches (e.g. the SCRIPT cache) persist across calls
/// within a test.
fn dispatch<A: kevy_rt::ArgvView + ?Sized>(store: &mut kevy_store::Store, args: &A) -> Vec<u8> {
    thread_local! {
        static KEVY: kevy::KevyCommands = kevy::KevyCommands::new();
    }
    KEVY.with(|k| k.dispatch(store, args))
}

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
    let reply = dispatch(
        &mut store,
        &argv(&[b"HMSET", b"h", b"f1", b"v1", b"f2", b"v2"]),
    );
    assert_eq!(reply, b"+OK\r\n");
    let g1 = dispatch(&mut store, &argv(&[b"HGET", b"h", b"f1"]));
    assert_eq!(g1, b"$2\r\nv1\r\n");
    let g2 = dispatch(&mut store, &argv(&[b"HGET", b"h", b"f2"]));
    assert_eq!(g2, b"$2\r\nv2\r\n");
}

#[test]
fn hmset_overwrites_existing_field() {
    let mut store = Store::new();
    dispatch(&mut store, &argv(&[b"HSET", b"h", b"f1", b"old"]));
    let reply = dispatch(&mut store, &argv(&[b"HMSET", b"h", b"f1", b"new"]));
    assert_eq!(reply, b"+OK\r\n");
    let g1 = dispatch(&mut store, &argv(&[b"HGET", b"h", b"f1"]));
    assert_eq!(g1, b"$3\r\nnew\r\n");
}

#[test]
fn hmset_wrong_arity_errors() {
    // odd-count: HMSET key f1 v1 f2 (missing value for f2).
    let mut store = Store::new();
    let reply = dispatch(&mut store, &argv(&[b"HMSET", b"h", b"f1", b"v1", b"f2"]));
    assert!(
        reply.starts_with(b"-ERR wrong number of arguments"),
        "got {:?}",
        String::from_utf8_lossy(&reply)
    );
}

#[test]
fn hmset_on_wrong_type_errors() {
    let mut store = Store::new();
    dispatch(&mut store, &argv(&[b"SET", b"s", b"str"]));
    let reply = dispatch(&mut store, &argv(&[b"HMSET", b"s", b"f", b"v"]));
    assert!(
        reply.starts_with(b"-WRONGTYPE "),
        "got {:?}",
        String::from_utf8_lossy(&reply)
    );
}

// ---- hash field TTLs --------------------------------------------------------

#[test]
fn hexpire_httl_hpersist_dispatch() {
    let mut store = Store::new();
    dispatch(&mut store, &argv(&[b"HSET", b"ht", b"a", b"1", b"b", b"2"]));
    // HEXPIRE with GT cond keyword + FIELDS
    let r = dispatch(
        &mut store,
        &argv(&[b"HEXPIRE", b"ht", b"100", b"FIELDS", b"2", b"a", b"nope"]),
    );
    assert_eq!(r, b"*2\r\n:1\r\n:-2\r\n");
    let r = dispatch(&mut store, &argv(&[b"HTTL", b"ht", b"FIELDS", b"2", b"a", b"b"]));
    let s = String::from_utf8_lossy(&r);
    assert!(s.starts_with("*2\r\n:"), "{s}");
    assert!(s.ends_with(":-1\r\n"), "b has no ttl: {s}");
    // NX refused on a
    let r = dispatch(
        &mut store,
        &argv(&[b"HEXPIRE", b"ht", b"200", b"NX", b"FIELDS", b"1", b"a"]),
    );
    assert_eq!(r, b"*1\r\n:0\r\n");
    // HPERSIST clears
    let r = dispatch(&mut store, &argv(&[b"HPERSIST", b"ht", b"FIELDS", b"1", b"a"]));
    assert_eq!(r, b"*1\r\n:1\r\n");
    // HPEXPIREAT with past deadline deletes (code 2)
    let r = dispatch(
        &mut store,
        &argv(&[b"HPEXPIREAT", b"ht", b"1", b"FIELDS", b"1", b"b"]),
    );
    assert_eq!(r, b"*1\r\n:2\r\n");
    let r = dispatch(&mut store, &argv(&[b"HEXISTS", b"ht", b"b"]));
    assert_eq!(r, b":0\r\n");
    // FIELDS keyword mandatory
    let r = dispatch(&mut store, &argv(&[b"HEXPIRE", b"ht", b"5", b"1", b"a"]));
    assert!(r.starts_with(b"-ERR"));
}
