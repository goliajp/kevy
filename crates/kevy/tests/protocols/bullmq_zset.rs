//! Sorted-set ops needed by BullMQ — ZPOPMIN,
//! ZREMRANGEBYRANK, ZREMRANGEBYSCORE, ZREVRANGEBYSCORE.

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

fn zadd(store: &mut Store, key: &[u8], pairs: &[(&[u8], &[u8])]) {
    // pairs: &[(score_bytes, member_bytes)]
    let mut parts: Vec<&[u8]> = vec![b"ZADD", key];
    for (s, m) in pairs {
        parts.push(s);
        parts.push(m);
    }
    dispatch(store, &argv(&parts));
}

// ---- ZPOPMIN -----------------------------------------------------

#[test]
fn zpopmin_returns_lowest_member_and_score() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"3", b"c"), (b"1", b"a"), (b"2", b"b")]);
    let r = dispatch(&mut store, &argv(&[b"ZPOPMIN", b"z"]));
    // *2  $1 a  $1 1
    assert_eq!(r, b"*2\r\n$1\r\na\r\n$1\r\n1\r\n");
    let card = dispatch(&mut store, &argv(&[b"ZCARD", b"z"]));
    assert_eq!(card, b":2\r\n");
}

#[test]
fn zpopmin_with_count_returns_n_pairs() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"3", b"c"), (b"1", b"a"), (b"2", b"b")]);
    let r = dispatch(&mut store, &argv(&[b"ZPOPMIN", b"z", b"2"]));
    // *4 a 1 b 2
    assert_eq!(r, b"*4\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n");
}

#[test]
fn zpopmin_empty_key_returns_empty_array() {
    let mut store = Store::new();
    let r = dispatch(&mut store, &argv(&[b"ZPOPMIN", b"absent"]));
    assert_eq!(r, b"*0\r\n");
}

#[test]
fn zpopmin_wrong_type_errors() {
    let mut store = Store::new();
    dispatch(&mut store, &argv(&[b"SET", b"s", b"x"]));
    let r = dispatch(&mut store, &argv(&[b"ZPOPMIN", b"s"]));
    assert!(r.starts_with(b"-WRONGTYPE "));
}

// ---- ZREMRANGEBYRANK ---------------------------------------------

#[test]
fn zremrangebyrank_removes_in_inclusive_range() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"1", b"a"), (b"2", b"b"), (b"3", b"c"), (b"4", b"d"), (b"5", b"e")]);
    let r = dispatch(&mut store, &argv(&[b"ZREMRANGEBYRANK", b"z", b"1", b"3"]));
    assert_eq!(r, b":3\r\n");
    let g = dispatch(&mut store, &argv(&[b"ZRANGE", b"z", b"0", b"-1", b"WITHSCORES"]));
    assert_eq!(g, b"*4\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\ne\r\n$1\r\n5\r\n");
}

#[test]
fn zremrangebyrank_negative_indices() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"1", b"a"), (b"2", b"b"), (b"3", b"c")]);
    // -2..=-1 = last two
    let r = dispatch(&mut store, &argv(&[b"ZREMRANGEBYRANK", b"z", b"-2", b"-1"]));
    assert_eq!(r, b":2\r\n");
    let g = dispatch(&mut store, &argv(&[b"ZRANGE", b"z", b"0", b"-1"]));
    assert_eq!(g, b"*1\r\n$1\r\na\r\n");
}

#[test]
fn zremrangebyrank_absent_key_returns_zero() {
    let mut store = Store::new();
    let r = dispatch(&mut store, &argv(&[b"ZREMRANGEBYRANK", b"absent", b"0", b"-1"]));
    assert_eq!(r, b":0\r\n");
}

// ---- ZREMRANGEBYSCORE --------------------------------------------

#[test]
fn zremrangebyscore_removes_inclusive_bounds() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"1", b"a"), (b"2", b"b"), (b"3", b"c"), (b"4", b"d")]);
    let r = dispatch(&mut store, &argv(&[b"ZREMRANGEBYSCORE", b"z", b"2", b"3"]));
    assert_eq!(r, b":2\r\n");
    let g = dispatch(&mut store, &argv(&[b"ZRANGE", b"z", b"0", b"-1"]));
    assert_eq!(g, b"*2\r\n$1\r\na\r\n$1\r\nd\r\n");
}

#[test]
fn zremrangebyscore_exclusive_bound_via_paren() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"1", b"a"), (b"2", b"b"), (b"3", b"c")]);
    // (1 .. 3 → b, c
    let r = dispatch(&mut store, &argv(&[b"ZREMRANGEBYSCORE", b"z", b"(1", b"3"]));
    assert_eq!(r, b":2\r\n");
}

#[test]
fn zremrangebyscore_inf_bounds() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"1", b"a"), (b"2", b"b"), (b"3", b"c")]);
    let r = dispatch(&mut store, &argv(&[b"ZREMRANGEBYSCORE", b"z", b"-inf", b"+inf"]));
    assert_eq!(r, b":3\r\n");
}

// ---- ZREVRANGEBYSCORE --------------------------------------------

#[test]
fn zrevrangebyscore_returns_descending_order() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"1", b"a"), (b"2", b"b"), (b"3", b"c")]);
    // max=3, min=1
    let r = dispatch(&mut store, &argv(&[b"ZREVRANGEBYSCORE", b"z", b"3", b"1"]));
    assert_eq!(r, b"*3\r\n$1\r\nc\r\n$1\r\nb\r\n$1\r\na\r\n");
}

#[test]
fn zrevrangebyscore_withscores_v2_shape() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"1", b"a"), (b"2", b"b")]);
    let r =
        dispatch(&mut store, &argv(&[b"ZREVRANGEBYSCORE", b"z", b"+inf", b"-inf", b"WITHSCORES"]));
    // *4 b 2 a 1
    assert_eq!(r, b"*4\r\n$1\r\nb\r\n$1\r\n2\r\n$1\r\na\r\n$1\r\n1\r\n");
}

#[test]
fn zrevrangebyscore_limit() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"1", b"a"), (b"2", b"b"), (b"3", b"c"), (b"4", b"d")]);
    let r = dispatch(
        &mut store,
        &argv(&[b"ZREVRANGEBYSCORE", b"z", b"+inf", b"-inf", b"LIMIT", b"1", b"2"]),
    );
    // Reversed order [d, c, b, a]; LIMIT 1 2 → [c, b]
    assert_eq!(r, b"*2\r\n$1\r\nc\r\n$1\r\nb\r\n");
}

#[test]
fn zrevrangebyscore_bad_bound_errors() {
    let mut store = Store::new();
    zadd(&mut store, b"z", &[(b"1", b"a")]);
    let r = dispatch(&mut store, &argv(&[b"ZREVRANGEBYSCORE", b"z", b"notafloat", b"-inf"]));
    assert!(r.starts_with(b"-ERR min or max"));
}

// ---- ZPOPMIN.BELOW (delayed-job pop) ---------------------------------------

#[test]
fn zpopmin_below_pops_only_due() {
    let mut store = Store::new();
    zadd(&mut store, b"delayed", &[(b"10", b"j1"), (b"20", b"j2"), (b"99", b"j3")]);
    // strictly below 20 → only j1
    let r = dispatch(&mut store, &argv(&[b"ZPOPMIN.BELOW", b"delayed", b"20"]));
    assert_eq!(r, b"*2\r\n$2\r\nj1\r\n$2\r\n10\r\n");
    // below 100 count 5 → j2 then j3
    let r = dispatch(&mut store, &argv(&[b"ZPOPMIN.BELOW", b"delayed", b"100", b"5"]));
    assert_eq!(r, b"*4\r\n$2\r\nj2\r\n$2\r\n20\r\n$2\r\nj3\r\n$2\r\n99\r\n");
    // drained
    let r = dispatch(&mut store, &argv(&[b"ZPOPMIN.BELOW", b"delayed", b"100"]));
    assert_eq!(r, b"*0\r\n");
    // arity + float errors
    let r = dispatch(&mut store, &argv(&[b"ZPOPMIN.BELOW", b"delayed"]));
    assert!(r.starts_with(b"-ERR"));
    let r = dispatch(&mut store, &argv(&[b"ZPOPMIN.BELOW", b"delayed", b"nan-ish"]));
    assert!(r.starts_with(b"-ERR"));
}
