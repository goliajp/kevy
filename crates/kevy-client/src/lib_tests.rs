use super::*;

/// Smoke against the embedded backend: every generic + string method
/// delegating to `Store`. Per-collection coverage (hash/list/set/zset)
/// lives in `collections::tests`.
#[test]
fn embedded_mem_full_crud_round_trip() {
    let mut c = Connection::open("mem://").unwrap();
    c.ping().unwrap();

    c.set(b"k", b"v").unwrap();
    assert_eq!(c.get(b"k").unwrap(), Some(b"v".to_vec()));

    assert_eq!(c.del(&[&b"k"[..], &b"missing"[..]]).unwrap(), 1);
    assert_eq!(c.get(b"k").unwrap(), None);

    c.set(b"a", b"1").unwrap();
    c.set(b"b", b"2").unwrap();
    assert_eq!(c.exists(&[&b"a"[..], &b"b"[..], &b"none"[..]]).unwrap(), 2);

    assert_eq!(c.incr(b"counter").unwrap(), 1);
    assert_eq!(c.incr_by(b"counter", 9).unwrap(), 10);

    c.set(b"timed", b"x").unwrap();
    assert!(c.expire(b"timed", Duration::from_mins(1)).unwrap());
    let ttl = c.ttl_ms(b"timed").unwrap();
    assert!((0..=60_000).contains(&ttl), "ttl_ms = {ttl}");
    assert!(c.persist(b"timed").unwrap());
    assert_eq!(c.ttl_ms(b"timed").unwrap(), -1);

    assert_eq!(c.type_of(b"none").unwrap(), "none");
    assert_eq!(c.type_of(b"timed").unwrap(), "string");

    assert!(c.dbsize().unwrap() >= 3);
    c.flushall().unwrap();
    assert_eq!(c.dbsize().unwrap(), 0);

    c.set_with_ttl(b"timed2", b"x", Duration::from_mins(1))
        .unwrap();
    let ttl = c.ttl_ms(b"timed2").unwrap();
    assert!((0..=60_000).contains(&ttl));
}

#[test]
fn anonymous_mem_publish_returns_zero() {
    // No bus, no subscribers — by design.
    let mut c = Connection::open("mem://").unwrap();
    assert_eq!(c.publish(b"chan", b"hi").unwrap(), 0);
}

#[test]
fn embedded_mget_mset() {
    let mut c = Connection::open("mem://").unwrap();
    c.mset(&[
        (b"a".as_ref(), b"1".as_ref()),
        (b"b".as_ref(), b"2".as_ref()),
    ])
    .unwrap();
    let got = c.mget(&[&b"a"[..], &b"b"[..], &b"missing"[..]]).unwrap();
    assert_eq!(
        got,
        vec![Some(b"1".to_vec()), Some(b"2".to_vec()), None]
    );
}

#[test]
fn embedded_multi_rejected_unsupported() {
    let mut c = Connection::open("mem://").unwrap();
    let err = c.multi().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}
